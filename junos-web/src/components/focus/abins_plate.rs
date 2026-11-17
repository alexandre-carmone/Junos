//! Aberration Inspector: the tilt plate seen from behind the camera, the
//! desktop Tilt Correction Advisory's drawing (tiltcorrectionwidget.cpp
//! `paintEvent`), with the screw table under it.
//!
//! The sensor is drawn fixed, its top up ("Camera top"), and mirrored left–
//! right against the image (the sensor map): from behind, the image's left
//! is on the right. Only the screw ring turns with the plate angle,
//! clockwise from behind. A screw at front-view angle `a` (`angleDeg`, the
//! plate angle included) sits at `(C − RS·cos a, C − RS·sin a)` — the
//! desktop's 3-point (1 upper right, 2 bottom, 3 upper left) and 4-point
//! (1 top left, 2 top right, 3 bottom left, 4 bottom right) layouts.
//!
//! A disc is red when the screw pushes out (+), blue when it backs off (−),
//! grey below 0.001 mm, as on the desktop. The fork gives signed mm only
//! (tiltcorrectioncalc.h), so no clockwise / counter-clockwise: that
//! depends on the thread.

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE};
use crate::i18n::Translations;
use crate::ws::{AbInsScrew, AbInsTiltPlate};

use super::abins::fact;

// The drawing, in SVG units: centre, plate radius, screw ring radius, disc
// radius; the sensor's half width and height (the desktop's 1.2 R × 0.8 R).
const S: f64 = 260.0;
const C: f64 = 130.0;
const RP: f64 = 100.0;
const RS: f64 = 78.0;
const RD: f64 = 11.0;
const SW: f64 = 60.0;
const SH: f64 = 40.0;

/// KStars' "no change" threshold (tiltcorrectioncalc.h `NO_CHANGE_MM`).
pub(super) const NO_CHANGE_MM: f64 = 0.001;

/// A screw's position on the plate, rear view (tiltcorrectioncalc.cpp).
pub(super) fn position_text(pos: &str, tr: &'static Translations) -> &'static str {
    match pos {
        "right" => tr.abins_pos_right,
        "upperRight" => tr.abins_pos_upper_right,
        "top" => tr.abins_pos_top,
        "upperLeft" => tr.abins_pos_upper_left,
        "left" => tr.abins_pos_left,
        "lowerLeft" => tr.abins_pos_lower_left,
        "bottom" => tr.abins_pos_bottom,
        "lowerRight" => tr.abins_pos_lower_right,
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

/// Disc colour, table class and direction of a screw's adjustment.
pub(super) fn direction(mm: f64, tr: &'static Translations) -> (&'static str, &'static str, &'static str) {
    if mm.abs() < NO_CHANGE_MM {
        ("var(--text-faint)", "text-text-faint", "\u{2014}")
    } else if mm > 0.0 {
        ("var(--band-ha)", "text-band-ha", tr.abins_dir_push)
    } else {
        ("var(--text-blue)", "text-text-blue", tr.abins_dir_back)
    }
}

/// One screw: its disc and number, mm and turns under it.
fn screw(s: &AbInsScrew, tr: &'static Translations) -> impl IntoView + use<> {
    let a = s.angle_deg.to_radians();
    let (x, y) = (C - RS * a.cos(), C - RS * a.sin());
    let (color, _, _) = direction(s.adjustment_mm, tr);
    let zero = s.adjustment_mm.abs() < NO_CHANGE_MM;
    let (mm, turns) = if zero {
        ("0".to_string(), String::new())
    } else {
        (format!("{:+.3} mm", s.adjustment_mm), format!("{:+.2} {}", s.turns, tr.abins_turns.to_lowercase()))
    };
    view! {
        <circle cx=x cy=y r=RD fill=color fill-opacity="0.3" stroke=color stroke-width="2" />
        <text x=x y=y fill="var(--text)" font-size="11" font-weight="bold" text-anchor="middle"
              dominant-baseline="central">{s.index.to_string()}</text>
        <g fill=color font-size="9" text-anchor="middle" stroke="var(--bg-input-deep)" stroke-width="3" paint-order="stroke">
            <text x=x y=y + RD + 10.0>{mm}</text>
            <text x=x y=y + RD + 20.0>{turns}</text>
        </g>
    }
}

/// The rear view.
fn diagram(p: &AbInsTiltPlate, tr: &'static Translations) -> impl IntoView + use<> {
    let screws = p.points.iter().map(|s| screw(s, tr)).collect::<Vec<_>>();
    let top = C - RP;
    view! {
        <svg viewBox=format!("0 0 {S} {S}")
             class="block w-full max-w-[320px] mx-auto rounded-md bg-bg-input-deep border border-border-base font-mono">
            <circle cx=C cy=C r=RP fill="var(--bg-elev-2)" stroke="var(--border-mid)" stroke-width="2" />
            // The sensor, mirrored: the image's left is on the right.
            <rect x=C - SW y=C - SH width=2.0 * SW height=2.0 * SH fill="none"
                  stroke="var(--text-faint)" stroke-width="1" stroke-dasharray="4 3" />
            <g fill="var(--text-faint)" font-size="8">
                <text x=C + SW - 4.0 y=C text-anchor="end" dominant-baseline="middle">{tr.abins_image_l}</text>
                <text x=C - SW + 4.0 y=C dominant-baseline="middle">{tr.abins_image_r}</text>
                <text x=C y=C + SH - 9.0 text-anchor="middle">{tr.abins_sensor_rear}</text>
            </g>
            // "Camera top": the sensor's top edge, fixed.
            <path d=format!("M{C} {b}V{t}M{l} {h}L{C} {t}L{r} {h}", b = top + 10.0, t = top - 4.0,
                            l = C - 4.0, r = C + 4.0, h = top + 2.0)
                  fill="none" stroke="var(--accent-amber)" stroke-width="2" />
            <text x=C y=top - 10.0 fill="var(--accent-amber)" font-size="9" text-anchor="middle">{tr.abins_camera_top}</text>
            {screws}
        </svg>
    }
}

/// The plate card from `advice` (the screws under the current settings).
pub(super) fn plate_card(
    advice: Option<&AbInsTiltPlate>,
    tilt_ok: bool,
    plate_present: bool,
    tr: &'static Translations,
) -> impl IntoView + use<> {
    let body = match advice {
        Some(p) => {
            let rows = p.points.iter().map(|s| {
                let (_, cls, dir) = direction(s.adjustment_mm, tr);
                view! {
                    <tr>
                        <td class="py-1 pr-2 font-mono">{format!("#{}", s.index)}</td>
                        <td class="py-1 pr-2">{position_text(&s.position, tr)}</td>
                        <td class=format!("py-1 pr-2 {cls}")>{dir}</td>
                        <td class="py-1 pr-2 font-mono text-right">{format!("{:+.3}", s.adjustment_mm)}</td>
                        <td class="py-1 font-mono text-right">{format!("{:+.2}", s.turns)}</td>
                    </tr>
                }
            }).collect_view();
            let view_line = if p.plate_angle_deg.abs() < 0.01 {
                tr.abins_plate_view.to_string()
            } else {
                format!("{} · {:.0}° {}", tr.abins_plate_view, p.plate_angle_deg.abs(),
                        if p.plate_angle_deg > 0.0 { tr.abins_plate_cw } else { tr.abins_plate_ccw })
            };
            view! {
                {fact(tr.abins_plate_type, format!("{} · {}",
                    if p.plate_type == 1 { tr.abins_plate_4 } else { tr.abins_plate_3 },
                    if p.mode == 1 { tr.abins_mode_push } else { tr.abins_mode_relative }))}
                {fact(tr.abins_geometry, format!("r {:.0} mm · {:.2} mm/turn · {:.0}°", p.radius_mm, p.screw_pitch_mm, p.plate_angle_deg))}
                {fact(tr.abins_total_tilt, format!("{:.0} µm", p.total_tilt_microns))}
                {fact(tr.abins_max_adj, format!("{:.3} mm", p.max_adjustment_mm))}
                {diagram(p, tr)}
                <div class="text-xs text-text-faint text-center">{view_line}</div>
                <table class="w-full text-sm">
                    <thead class="text-xs uppercase tracking-[0.06em] text-text-muted">
                        <tr>
                            <th class="text-left font-normal pr-2">{tr.abins_screw}</th>
                            <th class="text-left font-normal pr-2">{tr.abins_position}</th>
                            <th class="text-left font-normal pr-2">{tr.abins_direction}</th>
                            <th class="text-right font-normal pr-2">"mm"</th>
                            <th class="text-right font-normal">{tr.abins_turns}</th>
                        </tr>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>
                <div class="text-xs text-text-muted leading-tight">{tr.abins_plate_hint}</div>
            }.into_any()
        }
        None => view! {
            <div class="text-text-faint">
                {if !tilt_ok { tr.abins_plate_no_tilt } else if !plate_present { tr.abins_no_plate } else { tr.abins_not_ok }}
            </div>
        }.into_any(),
    };
    view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_tilt_plate}</span>
            {body}
        </div>
    }
}
