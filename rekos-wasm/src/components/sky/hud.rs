//! Bottom-left status HUD — Leptos DOM overlay.
//!
//! Replaces `render::render_info_overlay` (Canvas2D). The render Effect in
//! `mod.rs` writes a `HudData` snapshot every frame; this component reads it
//! reactively. Positioned absolute, `pointer-events:none` so the GPU canvas
//! beneath it still receives mouse/touch input.

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
    pub rotation_deg:   Option<f64>,
    pub t_off:          f64,
    pub cursor_altaz:   Option<(f64, f64)>,
    pub cursor_radec:   Option<(f64, f64)>,
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
    let line_rotation = move || {
        let h = hud.get();
        let tr = t(lang.get());
        h.rotation_deg.map(|r| format!("{}: {:.1}°", tr.overlay_camera_angle, r))
    };
    let line_t_off = move || {
        let h = hud.get();
        let tr = t(lang.get());
        if h.t_off.abs() > 0.5 {
            Some(format!("{}: {:+.0}s", tr.overlay_time_offset, h.t_off))
        } else {
            None
        }
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
        <div style="position:absolute;left:0;bottom:0;width:360px;\
                    background:rgba(10,10,20,0.75);color:#aabbcc;\
                    font:11px monospace;padding:8px 8px 6px 8px;\
                    line-height:16px;pointer-events:none;\
                    box-sizing:border-box;">
            <div>{line_lst_fov}</div>
            <div>{line_center}</div>
            <div>{line_mount}</div>
            { move || line_rotation().map(|s| view! { <div>{s}</div> }) }
            { move || line_t_off().map(|s| view! { <div>{s}</div> }) }
            { move || line_cursor().map(|s| view! { <div>{s}</div> }) }
        </div>
    }
}
