use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::MouseEvent;

use crate::compat::MountSnapshot;
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;

// ── Coordinate formatters ─────────────────────────────────────────────────────

fn fmt_hms(h: f64) -> String {
    let h = h.rem_euclid(24.0);
    let total_s = (h * 3600.0).round() as u64;
    let hh = total_s / 3600;
    let mm = (total_s % 3600) / 60;
    let ss = total_s % 60;
    format!("{hh:02}h {mm:02}m {ss:02}s")
}

fn fmt_dms(deg: f64) -> String {
    let sign = if deg < 0.0 { "-" } else { "+" };
    let abs = deg.abs();
    let dd = abs as u64;
    let mm = ((abs - dd as f64) * 60.0) as u64;
    let ss = ((abs - dd as f64) * 3600.0 - (mm as f64) * 60.0).round() as u64;
    format!("{sign}{dd:02}° {mm:02}' {ss:02}\"")
}

fn fmt_az(deg: f64) -> String {
    let d = deg.rem_euclid(360.0) as u64;
    let m = ((deg.rem_euclid(360.0) - d as f64) * 60.0) as u64;
    format!("{d:03}° {m:02}'")
}

// ── Slew rate labels ──────────────────────────────────────────────────────────

const RATE_LABELS: [&str; 8] = ["G", "1×", "2×", "4×", "8×", "16×", "32×", "MAX"];

// ── MountTab ──────────────────────────────────────────────────────────────────

#[component]
pub fn MountTab(mount: Signal<MountSnapshot>, send: SendCmd) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // ── GoTo input state ──────────────────────────────────────────────
    let ra_input  = RwSignal::new(String::new());
    let dec_input = RwSignal::new(String::new());
    let tgt_input = RwSignal::new(String::new());
    let j2000     = RwSignal::new(false);

    // ── Window width for responsive layout ───────────────────────────
    let win_w = RwSignal::new({
        web_sys::window()
            .and_then(|w| w.inner_width().ok())
            .and_then(|v| v.as_f64())
            .unwrap_or(1024.0)
    });
    let resize_send = win_w;
    let closure = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
        if let Some(w) = web_sys::window()
            .and_then(|w| w.inner_width().ok())
            .and_then(|v| v.as_f64())
        {
            resize_send.set(w);
        }
    });
    if let Some(window) = web_sys::window() {
        let _ = window.add_event_listener_with_callback(
            "resize",
            closure.as_ref().unchecked_ref(),
        );
    }
    closure.forget();

    let is_desktop = move || win_w.get() > 900.0;
    let is_phone   = move || win_w.get() < 600.0;

    // ── SendCmd clones for each action ────────────────────────────────
    let send = Arc::new(send);
    macro_rules! mk_send {
        () => { Arc::clone(&send) };
    }

    // Slew rate
    let send_rate = mk_send!();
    let on_rate = move |idx: i32| {
        let s = Arc::clone(&send_rate);
        move |_: MouseEvent| {
            s(serde_json::json!({
                "type": "mount_set_slew_rate",
                "payload": { "rate": idx }
            }).to_string());
        }
    };

    // GoTo / Sync
    let send_goto = mk_send!();
    let on_goto = {
        let ra = ra_input;
        let dec = dec_input;
        let j = j2000;
        move |_: MouseEvent| {
            send_goto(serde_json::json!({
                "type": "mount_goto_rade",
                "payload": { "ra": ra.get_untracked(), "de": dec.get_untracked(), "isJ2000": j.get_untracked() }
            }).to_string());
        }
    };
    let send_sync = mk_send!();
    let on_sync = {
        let ra = ra_input;
        let dec = dec_input;
        let j = j2000;
        move |_: MouseEvent| {
            send_sync(serde_json::json!({
                "type": "mount_sync_rade",
                "payload": { "ra": ra.get_untracked(), "de": dec.get_untracked(), "isJ2000": j.get_untracked() }
            }).to_string());
        }
    };
    let send_goto_target = mk_send!();
    let on_goto_target = {
        let tgt = tgt_input;
        move |_: MouseEvent| {
            send_goto_target(serde_json::json!({
                "type": "mount_goto_target",
                "payload": { "target": tgt.get_untracked() }
            }).to_string());
        }
    };

    let send_park    = mk_send!();
    let send_unpark  = mk_send!();
    let send_abort   = mk_send!();
    let send_abort2  = mk_send!();
    let send_track   = mk_send!();

    // ── Styles ────────────────────────────────────────────────────────
    let input_style =
        "width:100%; padding:6px 8px; background:#111; color:#ccc; \
         border:1px solid #444; border-radius:4px; font:13px monospace; \
         box-sizing:border-box;";

    let label_style = "font:11px monospace; color:#888; margin-bottom:2px; display:block;";

    let btn = |color: &str| -> String {
        format!(
            "padding:8px 12px; border-radius:6px; border:1px solid {color}; \
             background:rgba(20,22,40,0.92); color:{color}; font:700 12px monospace; \
             cursor:pointer; touch-action:manipulation; \
             -webkit-tap-highlight-color:transparent; \
             min-height:40px; width:100%;"
        )
    };

    let dpad_btn = move || {
        let sz = if is_phone() { "56px" } else { "48px" };
        format!(
            "width:{sz}; height:{sz}; border-radius:8px; border:1px solid #4466cc; \
             background:rgba(20,26,60,0.92); color:#99bbff; \
             font:700 18px/1 monospace; cursor:pointer; \
             touch-action:none; -webkit-tap-highlight-color:transparent; \
             display:flex; align-items:center; justify-content:center; \
             user-select:none;"
        )
    };

    let dpad_abort_btn = move || {
        let sz = if is_phone() { "56px" } else { "48px" };
        format!(
            "width:{sz}; height:{sz}; border-radius:50%; border:2px solid #ff4444; \
             background:rgba(60,10,10,0.92); color:#ff8888; \
             font:700 14px/1 monospace; cursor:pointer; \
             touch-action:none; -webkit-tap-highlight-color:transparent; \
             display:flex; align-items:center; justify-content:center; \
             user-select:none;"
        )
    };

    let section_title_style =
        "font:700 11px monospace; color:#88aaff; letter-spacing:0.1em; \
         border-bottom:1px solid #223; padding-bottom:4px; margin-bottom:10px;";

    view! {
        <div
            style="position:absolute; inset:0; background:#0a0a0f; color:#c0c0d0; \
                   font-family:monospace; overflow-y:auto; overflow-x:hidden; \
                   padding-bottom:60px;"
            on:click=|ev: MouseEvent| ev.stop_propagation()
        >
            // ── Header ────────────────────────────────────────────────
            <div style="display:flex; align-items:center; gap:10px; padding:10px 14px 6px; \
                        border-bottom:1px solid #222; flex-wrap:wrap;">
                <span style="font:700 13px monospace; color:#cfe0ff; letter-spacing:0.08em;">
                    {move || tr().mount_title}
                </span>
                {move || {
                    let m = mount.get();
                    m.device_name.map(|dev| view! {
                        <span style="font:12px monospace; color:#666; margin-left:4px;">{dev}</span>
                    })
                }}
                <div style="margin-left:auto;">
                    {move || {
                        let m = mount.get();
                        let (label, color) = status_label_color(&m, tr());
                        view! {
                            <span style=format!(
                                "padding:3px 8px; border-radius:4px; border:1px solid {color}; \
                                 background:rgba(0,0,0,0.5); color:{color}; \
                                 font:700 11px monospace; letter-spacing:0.08em;"
                            )>{label}</span>
                        }
                    }}
                </div>
            </div>

            // ── Info banners ──────────────────────────────────────────
            {move || {
                let m = mount.get();
                (!m.meridian_flip_status.is_empty()).then(|| view! {
                    <div style="padding:4px 14px; background:rgba(60,40,0,0.6); \
                                border-bottom:1px solid #664; \
                                font:11px monospace; color:#ffcc88;">
                        {tr().mount_meridian_flip}{": "}{m.meridian_flip_status}
                    </div>
                })
            }}
            {move || {
                let m = mount.get();
                (!m.auto_park_countdown.is_empty()).then(|| view! {
                    <div style="padding:4px 14px; background:rgba(20,40,80,0.6); \
                                border-bottom:1px solid #336; \
                                font:11px monospace; color:#88aaff;">
                        {tr().mount_autopark}{": "}{m.auto_park_countdown}
                    </div>
                })
            }}

            // ── Body — responsive grid ────────────────────────────────
            <div style=move || {
                if is_desktop() {
                    "display:grid; grid-template-columns:1fr 1fr; gap:0; height:calc(100% - 80px);"
                        .to_string()
                } else {
                    "display:flex; flex-direction:column;".to_string()
                }
            }>

                // ── Left column: Coordinates + GoTo ───────────────────
                <div style="padding:14px; overflow-y:auto; border-right:1px solid #1a1a2e;">

                    // Coordinates section
                    <div style=section_title_style>{move || tr().mount_coords_section}</div>

                    // No-mount placeholder
                    {move || {
                        let m = mount.get();
                        (!m.connected).then(|| view! {
                            <div style="color:#555; font:12px monospace; padding:20px 0; text-align:center;">
                                {tr().mount_no_device}
                            </div>
                        })
                    }}

                    // Coordinate rows
                    {move || {
                        let m = mount.get();
                        m.connected.then(|| {
                            let coords: Vec<(&'static str, String)> = vec![
                                (tr().mount_ra_jnow,
                                 m.ra_h.map(fmt_hms).unwrap_or_else(|| "—".into())),
                                (tr().mount_dec_jnow,
                                 m.dec_deg.map(fmt_dms).unwrap_or_else(|| "—".into())),
                                (tr().mount_ra_j2000,
                                 m.ra0_h.map(fmt_hms).unwrap_or_else(|| "—".into())),
                                (tr().mount_dec_j2000,
                                 m.dec0_deg.map(fmt_dms).unwrap_or_else(|| "—".into())),
                                (tr().mount_az,
                                 m.az_deg.map(fmt_az).unwrap_or_else(|| "—".into())),
                                (tr().mount_alt,
                                 m.alt_deg.map(|v| fmt_dms(v)).unwrap_or_else(|| "—".into())),
                                (tr().mount_ha,
                                 m.ha_deg.map(|h| {
                                     let sign = if h < 0.0 { "-" } else { "+" };
                                     let h_abs = h.abs();
                                     let hh = (h_abs / 15.0) as u64;
                                     let mm = ((h_abs / 15.0 - hh as f64) * 60.0) as u64;
                                     format!("{sign}{hh:02}h {mm:02}m")
                                 }).unwrap_or_else(|| "—".into())),
                                (tr().mount_pier_side,
                                 match m.pier_side {
                                     Some(0)  => tr().mount_pier_west.to_string(),
                                     Some(1)  => tr().mount_pier_east.to_string(),
                                     _        => tr().mount_pier_unknown.to_string(),
                                 }),
                            ];
                            view! {
                                <div style="display:grid; grid-template-columns:auto 1fr; \
                                            gap:4px 12px; align-items:baseline;">
                                    {coords.into_iter().map(|(lbl, val)| view! {
                                        <span style="font:11px monospace; color:#888; white-space:nowrap;">
                                            {lbl}{":"}
                                        </span>
                                        <span style="font:12px monospace; color:#cfe0ff; \
                                                     font-variant-numeric:tabular-nums;">
                                            {val}
                                        </span>
                                    }).collect::<Vec<_>>()}
                                </div>
                            }
                        })
                    }}

                    // GoTo section
                    <div style="margin-top:18px;">
                        <div style=section_title_style>{move || tr().mount_goto_section}</div>
                        <div style="display:flex; flex-direction:column; gap:8px;">
                            <div>
                                <label style=label_style>{move || tr().mount_ra_input}</label>
                                <input
                                    type="text"
                                    placeholder="HH MM SS"
                                    style=input_style
                                    prop:value=move || ra_input.get()
                                    on:input=move |ev| ra_input.set(event_target_value(&ev))
                                />
                            </div>
                            <div>
                                <label style=label_style>{move || tr().mount_dec_input}</label>
                                <input
                                    type="text"
                                    placeholder="±DD MM SS"
                                    style=input_style
                                    prop:value=move || dec_input.get()
                                    on:input=move |ev| dec_input.set(event_target_value(&ev))
                                />
                            </div>
                            <div>
                                <label style=label_style>{move || tr().mount_target_input}</label>
                                <input
                                    type="text"
                                    placeholder="M42, NGC1234…"
                                    style=input_style
                                    prop:value=move || tgt_input.get()
                                    on:input=move |ev| tgt_input.set(event_target_value(&ev))
                                />
                            </div>
                            <label style="display:flex; align-items:center; gap:6px; \
                                          font:12px monospace; color:#aaa; cursor:pointer;">
                                <input
                                    type="checkbox"
                                    prop:checked=move || j2000.get()
                                    on:change=move |ev| j2000.set(event_target_checked(&ev))
                                />
                                {move || tr().mount_j2000_label}
                            </label>
                            <div style="display:grid; grid-template-columns:1fr 1fr; gap:6px;">
                                <button
                                    style=btn("#44cc88")
                                    on:click=on_goto.clone()
                                >{move || tr().mount_goto_btn}</button>
                                <button
                                    style=btn("#88aaff")
                                    on:click=on_sync.clone()
                                >{move || tr().mount_sync_btn}</button>
                            </div>
                            <button
                                style=btn("#66aaee")
                                on:click=on_goto_target.clone()
                            >{move || tr().mount_goto_target_btn}</button>
                        </div>
                    </div>
                </div>

                // ── Right column: D-pad + controls ────────────────────
                <div style="padding:14px; display:flex; flex-direction:column; gap:16px; \
                             align-items:center;">

                    // D-pad
                    <div>
                        <div style=move || format!(
                            "display:grid; grid-template-columns:repeat(3,{}); \
                             grid-template-rows:repeat(3,{}); gap:6px;",
                            if is_phone() { "56px" } else { "48px" },
                            if is_phone() { "56px" } else { "48px" }
                        )>
                            // Row 1: empty, N, empty
                            <div></div>
                            <button
                                style=move || dpad_btn()
                                on:pointerdown={
                                    let s = Arc::clone(&send);
                                    move |ev: web_sys::PointerEvent| {
                                        ev.prevent_default();
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"N","action":true}}).to_string());
                                    }
                                }
                                on:pointerup={
                                    let s = Arc::clone(&send);
                                    move |_: web_sys::PointerEvent| {
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"N","action":false}}).to_string());
                                    }
                                }
                                on:pointerleave={
                                    let s = Arc::clone(&send);
                                    move |_: web_sys::PointerEvent| {
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"N","action":false}}).to_string());
                                    }
                                }
                            >"↑"</button>
                            <div></div>

                            // Row 2: W, abort, E
                            <button
                                style=move || dpad_btn()
                                on:pointerdown={
                                    let s = Arc::clone(&send);
                                    move |ev: web_sys::PointerEvent| {
                                        ev.prevent_default();
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"W","action":true}}).to_string());
                                    }
                                }
                                on:pointerup={
                                    let s = Arc::clone(&send);
                                    move |_: web_sys::PointerEvent| {
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"W","action":false}}).to_string());
                                    }
                                }
                                on:pointerleave={
                                    let s = Arc::clone(&send);
                                    move |_: web_sys::PointerEvent| {
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"W","action":false}}).to_string());
                                    }
                                }
                            >"←"</button>
                            <button
                                style=move || dpad_abort_btn()
                                on:click=move |_| {
                                    send_abort2(serde_json::json!({"type":"mount_abort","payload":{}}).to_string());
                                }
                                title=move || tr().mount_abort_btn
                            >"●"</button>
                            <button
                                style=move || dpad_btn()
                                on:pointerdown={
                                    let s = Arc::clone(&send);
                                    move |ev: web_sys::PointerEvent| {
                                        ev.prevent_default();
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"E","action":true}}).to_string());
                                    }
                                }
                                on:pointerup={
                                    let s = Arc::clone(&send);
                                    move |_: web_sys::PointerEvent| {
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"E","action":false}}).to_string());
                                    }
                                }
                                on:pointerleave={
                                    let s = Arc::clone(&send);
                                    move |_: web_sys::PointerEvent| {
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"E","action":false}}).to_string());
                                    }
                                }
                            >"→"</button>

                            // Row 3: empty, S, empty
                            <div></div>
                            <button
                                style=move || dpad_btn()
                                on:pointerdown={
                                    let s = Arc::clone(&send);
                                    move |ev: web_sys::PointerEvent| {
                                        ev.prevent_default();
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"S","action":true}}).to_string());
                                    }
                                }
                                on:pointerup={
                                    let s = Arc::clone(&send);
                                    move |_: web_sys::PointerEvent| {
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"S","action":false}}).to_string());
                                    }
                                }
                                on:pointerleave={
                                    let s = Arc::clone(&send);
                                    move |_: web_sys::PointerEvent| {
                                        s(serde_json::json!({"type":"mount_set_motion","payload":{"direction":"S","action":false}}).to_string());
                                    }
                                }
                            >"↓"</button>
                            <div></div>
                        </div>
                    </div>

                    // Slew rate selector
                    <div style="width:100%; max-width:320px;">
                        <div style=format!("{section_title_style} margin-bottom:6px;")>
                            {move || tr().mount_slew_rate}
                        </div>
                        <div style="display:flex; gap:4px; flex-wrap:wrap; justify-content:center;">
                            {(0..8i32).map(|idx| {
                                let lbl = RATE_LABELS[idx as usize];
                                let oc = on_rate(idx);
                                view! {
                                    <button
                                        style=move || {
                                            let active = mount.get().slew_rate == Some(idx);
                                            let (bg, border, color) = if active {
                                                ("rgba(40,60,110,0.95)", "#88aaff", "#cfe0ff")
                                            } else {
                                                ("rgba(12,14,24,0.85)", "#2a2a35", "#88aaff")
                                            };
                                            format!(
                                                "min-width:36px; height:32px; padding:0 6px; \
                                                 border-radius:5px; border:1px solid {border}; \
                                                 background:{bg}; color:{color}; \
                                                 font:700 11px monospace; cursor:pointer; \
                                                 touch-action:manipulation; \
                                                 -webkit-tap-highlight-color:transparent;"
                                            )
                                        }
                                        on:click=oc
                                    >{lbl}</button>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </div>

                    // Action buttons
                    <div style="width:100%; max-width:320px; \
                                display:grid; grid-template-columns:1fr 1fr; gap:8px;">
                        <button
                            style=btn("#88aaff")
                            on:click=move |_| {
                                send_park(serde_json::json!({"type":"mount_park","payload":{}}).to_string());
                            }
                        >{move || tr().mount_park_btn}</button>
                        <button
                            style=btn("#66ccaa")
                            on:click=move |_| {
                                send_unpark(serde_json::json!({"type":"mount_unpark","payload":{}}).to_string());
                            }
                        >{move || tr().mount_unpark_btn}</button>
                        <button
                            style=btn("#ff5555")
                            on:click=move |_| {
                                send_abort(serde_json::json!({"type":"mount_abort","payload":{}}).to_string());
                            }
                        >{move || tr().mount_abort_btn}</button>
                        <button
                            style=move || {
                                let tracking = mount.get().tracking;
                                let color = if tracking { "#44ff88" } else { "#888" };
                                btn(color)
                            }
                            on:click=move |_| {
                                let enabled = !mount.get_untracked().tracking;
                                send_track(serde_json::json!({
                                    "type": "mount_set_tracking",
                                    "payload": { "enabled": enabled }
                                }).to_string());
                            }
                        >{move || if mount.get().tracking { tr().mount_tracking_on } else { tr().mount_tracking_off }}</button>
                    </div>
                </div>
            </div>
        </div>
    }
}

// ── Status label helper ───────────────────────────────────────────────────────

fn status_label_color<'a>(
    m: &MountSnapshot,
    tr: &'a crate::i18n::Translations,
) -> (&'a str, &'static str) {
    if !m.connected {
        return (tr.disconnected, "#555");
    }
    if m.slewing {
        return (tr.mount_status_slewing, "#ffaa44");
    }
    if m.tracking {
        return (tr.mount_status_tracking, "#44ff88");
    }
    if m.parked {
        return (tr.mount_status_parked, "#88aaff");
    }
    // Check raw status string for parking
    let s = m.status_str.to_lowercase();
    if s.contains("parking") {
        return (tr.mount_status_parking, "#6699ff");
    }
    if s.contains("error") {
        return (tr.mount_status_error, "#ff4444");
    }
    (tr.mount_status_idle, "#666")
}
