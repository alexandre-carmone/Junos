//! The native Guide tab's star profile card (PHD2's Star Profile window):
//! from the `+J` frame's `star_image` crop, the row (cyan) and the column
//! (amber) through the star, the background and the half maximum as dashed
//! lines, and the crop itself, log-stretched, on the right. The numbers are
//! the daemon's (peak, mass, SNR, HFD, saturation); the FWHM is measured
//! here on the row, as the daemon sends none. Deselect and Auto-select
//! below.

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::CanvasRenderingContext2d;

use crate::components::form::{CARD, CARD_TITLE};
use crate::i18n::{t, Lang};
use crate::ws::{GuiderFrame, GuiderStarImage};

use super::protocol::{Reply, Rpc};

/// The median of the crop's border pixels: the sky around the star.
fn background(si: &GuiderStarImage) -> f64 {
    let px = |x: usize, y: usize| f64::from(si.pixels[y * si.w + x]);
    let mut b: Vec<f64> = (0..si.w).flat_map(|x| [px(x, 0), px(x, si.h - 1)]).collect();
    b.extend((1..si.h.saturating_sub(1)).flat_map(|y| [px(0, y), px(si.w - 1, y)]));
    b.sort_by(f64::total_cmp);
    b.get(b.len() / 2).copied().unwrap_or(0.0)
}

/// The width at `half` of the peak of `row` nearest `cx`, interpolated
/// between the samples either side of each crossing.
fn fwhm(row: &[f64], half: f64, cx: usize) -> Option<f64> {
    let lo = cx.saturating_sub(2);
    let peak = (lo..(cx + 3).min(row.len())).max_by(|a, b| row[*a].total_cmp(&row[*b]))?;
    if row[peak] <= half {
        return None;
    }
    let cross = |a: usize, b: usize| a as f64 + (half - row[a]) / (row[b] - row[a]) * (b as f64 - a as f64);
    let left = (1..=peak).rev().find(|&i| row[i - 1] < half).map(|i| cross(i - 1, i))?;
    let right = (peak..row.len() - 1).find(|&i| row[i + 1] < half).map(|i| cross(i, i + 1))?;
    Some(right - left)
}

/// What the card shows of one crop.
struct Profile {
    row: Vec<f64>,
    col: Vec<f64>,
    bg: f64,
    peak: f64,
    fwhm: Option<f64>,
}

fn profile(si: &GuiderStarImage) -> Option<Profile> {
    if si.w < 3 || si.h < 3 || si.pixels.len() < si.w * si.h {
        return None;
    }
    let (cx, cy) = (si.pos.0.round().clamp(0.0, (si.w - 1) as f64) as usize, si.pos.1.round().clamp(0.0, (si.h - 1) as f64) as usize);
    let row: Vec<f64> = (0..si.w).map(|x| f64::from(si.pixels[cy * si.w + x])).collect();
    let col: Vec<f64> = (0..si.h).map(|y| f64::from(si.pixels[y * si.w + cx])).collect();
    let bg = background(si);
    let peak = row.iter().chain(&col).copied().fold(bg, f64::max);
    let fwhm = fwhm(&row, bg + (peak - bg) / 2.0, cx);
    Some(Profile { row, col, bg, peak, fwhm })
}

fn draw(ctx: &CanvasRenderingContext2d, si: &GuiderStarImage, p: &Profile, cw: f64, ch: f64) {
    // The crop, log-stretched, as a square on the right.
    let side = ch;
    let (lo, hi) = si.pixels.iter().fold((u16::MAX, 0u16), |(a, b), v| (a.min(*v), b.max(*v)));
    let span = (f64::from(hi) - f64::from(lo)).max(1.0).ln_1p();
    let cell = side / si.w.max(si.h) as f64;
    let x0 = cw - side;
    for y in 0..si.h {
        for x in 0..si.w {
            let v = (f64::from(si.pixels[y * si.w + x]) - f64::from(lo)).ln_1p() / span;
            let g = (v.clamp(0.0, 1.0) * 255.0) as u8;
            ctx.set_fill_style_str(&format!("rgb({g},{g},{g})"));
            ctx.fill_rect(x0 + x as f64 * cell, y as f64 * cell, cell.ceil(), cell.ceil());
        }
    }

    // The cross-sections, from the lowest sample to the peak.
    let w = (x0 - 8.0).max(10.0);
    let floor = p.row.iter().chain(&p.col).copied().fold(p.bg, f64::min);
    let range = (p.peak - floor).max(1.0);
    let (top, bottom) = (4.0, ch - 4.0);
    let y_of = |v: f64| bottom - (v - floor) / range * (bottom - top);
    let dash = |d: &[f64]| {
        let arr = web_sys::js_sys::Array::new();
        for v in d {
            arr.push(&(*v).into());
        }
        let _ = ctx.set_line_dash(&arr);
    };
    let hline = |v: f64, color: &str| {
        ctx.set_stroke_style_str(color);
        ctx.begin_path();
        ctx.move_to(0.0, y_of(v));
        ctx.line_to(w, y_of(v));
        ctx.stroke();
    };
    ctx.set_line_width(1.0);
    dash(&[4.0, 3.0]);
    hline(p.bg, "rgba(160,170,190,0.7)");
    hline(p.bg + (p.peak - p.bg) / 2.0, "rgba(74,222,128,0.8)");
    dash(&[]);
    let curve = |vals: &[f64], color: &str, lw: f64| {
        let n = (vals.len().max(2) - 1) as f64;
        ctx.set_stroke_style_str(color);
        ctx.set_line_width(lw);
        ctx.begin_path();
        for (i, v) in vals.iter().enumerate() {
            let (x, y) = (i as f64 / n * w, y_of(*v));
            if i == 0 { ctx.move_to(x, y) } else { ctx.line_to(x, y) }
        }
        ctx.stroke();
    };
    curve(&p.col, "rgba(245,185,66,0.8)", 1.0);
    curve(&p.row, "rgba(120,235,255,0.95)", 2.0);
}

/// The card. `last` takes the buttons' calls (the footer shows an error);
/// `scale` (″/px) adds arc-seconds to the HFD and FWHM.
pub fn star_profile(
    frame: RwSignal<Option<GuiderFrame>>,
    rpc: Rpc,
    last: RwSignal<Option<Reply>>,
    scale: Memo<Option<f64>>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let canvas_ref = NodeRef::<html::Canvas>::new();
    let prof = Memo::new(move |_| frame.with(|f| f.as_ref().and_then(|f| f.star_image.as_ref()).and_then(profile).map(|p| p.fwhm)));
    let star = Memo::new(move |_| frame.with(|f| f.as_ref().and_then(|f| f.star.clone())));

    Effect::new(move |_| {
        let Some(canvas) = canvas_ref.get() else { return };
        frame.with(|f| {
            let Some(si) = f.as_ref().and_then(|f| f.star_image.as_ref()) else { return };
            let Some(p) = profile(si) else { return };
            let (cw, ch) = (f64::from(canvas.client_width()), f64::from(canvas.client_height()));
            if cw <= 0.0 || ch <= 0.0 {
                return;
            }
            let dpr = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio()).clamp(1.0, 2.0);
            canvas.set_width((cw * dpr).round() as u32);
            canvas.set_height((ch * dpr).round() as u32);
            let Ok(Some(ctx)) = canvas.get_context("2d") else { return };
            let ctx: CanvasRenderingContext2d = ctx.unchecked_into();
            let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
            ctx.clear_rect(0.0, 0.0, cw, ch);
            draw(&ctx, si, &p, cw, ch);
        });
    });

    let px_arcsec = move |v: f64| match scale.get() {
        Some(k) => format!("{v:.2} px \u{00b7} {:.2}\u{2033}", v * k),
        None => format!("{v:.2} px"),
    };
    let num = move |label: &'static str, value: String| view! {
        <div class="min-w-0 flex flex-col">
            <span class="text-[11px] uppercase tracking-[0.06em] text-text-muted truncate">{label}</span>
            <span class="font-mono text-sm text-text-dim truncate">{value}</span>
        </div>
    };
    let (rpc2, dash) = (rpc.clone(), || "\u{2014}".to_string());
    view! {
        <div class=CARD>
            <div class="flex items-center gap-2">
                <span class=format!("{CARD_TITLE} flex-1")>{move || tr().guider_profile_title}</span>
                <Show when=move || star.with(|s| s.as_ref().is_some_and(|s| s.saturated))>
                    <span class="badge badge--err">{move || tr().guider_saturated}</span>
                </Show>
            </div>
            <Show when=move || frame.with(|f| f.as_ref().is_some_and(|f| f.star_image.is_some()))
                  fallback=move || view! {
                      <div class="text-sm text-text-faint py-4 text-center">{move || tr().guider_profile_no_star}</div>
                  }>
                <canvas node_ref=canvas_ref class="w-full h-28 md:h-24" title=move || tr().guider_profile_hint></canvas>
            </Show>
            {move || {
                let tr = tr();
                let s = star.get();
                let f = |g: fn(&crate::ws::GuiderStar) -> f64, d: usize| s.as_ref().map_or_else(dash, |s| format!("{:.*}", d, g(s)));
                view! {
                    <div class="grid grid-cols-3 gap-x-3 gap-y-1">
                        {num(tr.guider_peak, f(|s| s.peak, 0))}
                        {num(tr.guider_mass, f(|s| s.mass, 0))}
                        {num(tr.guider_snr, f(|s| s.snr, 1))}
                        {num(tr.guider_hfd, s.as_ref().map_or_else(dash, |s| px_arcsec(s.hfd)))}
                        {num(tr.guider_fwhm, prof.get().flatten().map_or_else(dash, px_arcsec))}
                    </div>
                }
            }}
            <div class="flex gap-2">
                <button class="btn btn-ghost h-9 flex-1" disabled=move || star.with(Option::is_none)
                        on:click=move |_| last.set(Some(rpc.deselect_star()))>
                    {move || tr().guider_btn_deselect}
                </button>
                <button class="btn btn-ghost h-9 flex-1" on:click=move |_| last.set(Some(rpc2.find_star()))>
                    {move || tr().guider_btn_autoselect}
                </button>
            </div>
        </div>
    }
}
