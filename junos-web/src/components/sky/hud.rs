//! Bottom-left status HUD — Leptos DOM overlay (the time bar shows the time).
//!
//! The render Effect in `mod.rs` writes a `HudData` snapshot every frame (GPU
//! and Canvas2D-fallback alike); this component reads it reactively.
//! `pointer-events:none` so the canvas beneath still receives input.

use leptos::prelude::*;

use crate::i18n::{Lang, t};

#[derive(Clone, Debug, Default)]
pub struct HudData {
    pub lst_deg:        f64,
    pub fov:            f64,
    pub c_alt:          f64,
    pub c_az:           f64,
    pub mount_ra_h:     Option<f64>,
    pub mount_dec_deg:  Option<f64>,
    pub frame:          Option<HudFrame>,
    pub cursor_altaz:   Option<(f64, f64)>,
    pub cursor_radec:   Option<(f64, f64)>,
}

/// Where the camera frame shown in the HUD comes from (`Date::now()` ms).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FrameOrigin {
    Solved(f64),
    Saved(f64),
    Nominal,
}

/// The camera frame the FOV boxes draw, measured or nominal.
#[derive(Clone, Debug)]
pub struct HudFrame {
    pub fov_arcmin:     (f64, f64),
    pub pa_deg:         Option<f64>,
    pub origin:         FrameOrigin,
    /// Scope focal × CCD_INFO frame, to flag a measured frame that disagrees.
    pub nominal_arcmin: Option<(f64, f64)>,
}

/// Measured / nominal width outside this band → the mismatch warning.
const MISMATCH_TOLERANCE: f64 = 0.10;

/// "21:43" for today's solves, "2026-10-01" for saved ones (local time).
fn fmt_origin_time(ms: f64, with_date: bool) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms));
    if with_date {
        format!("{:04}-{:02}-{:02}", d.get_full_year(), d.get_month() + 1, d.get_date())
    } else {
        format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
    }
}

#[component]
pub fn SkyHud(
    hud: ReadSignal<HudData>,
    lang: ReadSignal<Lang>,
) -> impl IntoView {
    let line_lst_fov = move || {
        let h = hud.get();
        let tr = t(lang.get());
        let lst_h = h.lst_deg / 15.0;
        let lst_hh = lst_h as u32;
        let lst_mm = ((lst_h - lst_hh as f64) * 60.0) as u32;
        format!("{}: {:02}h{:02}m  {}: {:.0}°",
                tr.overlay_lst, lst_hh, lst_mm, tr.overlay_fov, h.fov)
    };
    let line_center = move || {
        let h = hud.get();
        let tr = t(lang.get());
        format!("{}: {} {:.1}°  {} {:.1}°",
                tr.overlay_center, tr.overlay_alt, h.c_alt, tr.overlay_az, h.c_az)
    };
    let line_mount = move || {
        let h = hud.get();
        let tr = t(lang.get());
        match (h.mount_ra_h, h.mount_dec_deg) {
            (Some(ra_h), Some(dec)) => {
                let rah = ra_h as u32;
                let ram = ((ra_h - rah as f64) * 60.0) as u32;
                format!("{}: {:02}h{:02}m  {:+.1}°", tr.overlay_mount, rah, ram, dec)
            }
            _ => tr.overlay_mount_none.to_string(),
        }
    };
    let line_frame = move || {
        let h = hud.get();
        let tr = t(lang.get());
        let f = h.frame?;
        let (w, ht) = f.fov_arcmin;
        let pa = f.pa_deg.map(|pa| format!(" · PA {pa:.1}°")).unwrap_or_default();
        let origin = match f.origin {
            FrameOrigin::Solved(ms) => format!("{} {}", tr.overlay_frame_solved, fmt_origin_time(ms, false)),
            FrameOrigin::Saved(ms) => format!("{} {}", tr.overlay_frame_saved, fmt_origin_time(ms, true)),
            FrameOrigin::Nominal => tr.overlay_frame_nominal.to_string(),
        };
        Some(format!("{}: {w:.0}'×{ht:.0}'{pa} ({origin})", tr.overlay_frame))
    };
    let line_mismatch = move || {
        let h = hud.get();
        let tr = t(lang.get());
        let f = h.frame?;
        let (nw, nh) = f.nominal_arcmin?;
        if f.origin == FrameOrigin::Nominal || nw <= 0.0 {
            return None;
        }
        let ratio = f.fov_arcmin.0 / nw;
        ((ratio - 1.0).abs() > MISMATCH_TOLERANCE).then(|| format!(
            "×{ratio:.2} vs {} {nw:.0}'×{nh:.0}' — {}",
            tr.overlay_frame_nominal, tr.overlay_frame_mismatch,
        ))
    };
    let line_cursor = move || {
        let h = hud.get();
        let tr = t(lang.get());
        match (h.cursor_altaz, h.cursor_radec) {
            (Some((alt, az)), Some((ra, dec))) => {
                let ra_h = ra / 15.0;
                let rah = ra_h as u32;
                let ram = ((ra_h - rah as f64) * 60.0) as u32;
                Some(format!(
                    "{}: {} {:+.1}° {} {:.1}°  {:02}h{:02}m {:+.1}°",
                    tr.overlay_cursor, tr.overlay_alt, alt, tr.overlay_az, az,
                    rah, ram, dec,
                ))
            }
            _ => None,
        }
    };

    view! {
        <div class="panel-glass max-w-full md:w-[360px] \
                    text-text-muted font-mono text-sm leading-4 \
                    px-sp-3 py-sp-2 pointer-events-none box-border">
            <div>{line_lst_fov}</div>
            // Phones: no hover cursor, and the centre is what you see.
            <div class="max-md:hidden">{line_center}</div>
            <div>{line_mount}</div>
            { move || line_frame().map(|s| view! { <div>{s}</div> }) }
            { move || line_mismatch().map(|s| view! { <div class="text-state-warn">{s}</div> }) }
            { move || line_cursor().map(|s| view! { <div class="max-md:hidden">{s}</div> }) }
        </div>
    }
}
