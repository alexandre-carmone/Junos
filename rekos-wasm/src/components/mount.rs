use std::sync::Arc;

use leptos::prelude::*;
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

    // D-pad press/release helpers — emit `mount_set_motion` for one direction.
    let dpad_press = |dir: &'static str| {
        let s = Arc::clone(&send);
        move |ev: web_sys::PointerEvent| {
            ev.prevent_default();
            s(serde_json::json!({
                "type": "mount_set_motion",
                "payload": { "direction": dir, "action": true }
            }).to_string());
        }
    };
    let dpad_release = |dir: &'static str| {
        let s = Arc::clone(&send);
        move |_: web_sys::PointerEvent| {
            s(serde_json::json!({
                "type": "mount_set_motion",
                "payload": { "direction": dir, "action": false }
            }).to_string());
        }
    };

    view! {
        <div
            class="mount-pane"
            on:click=|ev: MouseEvent| ev.stop_propagation()
        >
            // ── Header ────────────────────────────────────────────────
            <div class="mount-header">
                <span class="mount-header-title">
                    {move || tr().mount_title}
                </span>
                {move || {
                    let m = mount.get();
                    m.device_name.map(|dev| view! {
                        <span class="mount-header-device">{dev}</span>
                    })
                }}
                <div class="mount-header-status-wrap">
                    {move || {
                        let m = mount.get();
                        let (label, color) = status_label_color(&m, tr());
                        view! {
                            <span
                                class="mount-status-pill"
                                style=format!("color:{color};")
                            >{label}</span>
                        }
                    }}
                </div>
            </div>

            // ── Info banners ──────────────────────────────────────────
            {move || {
                let m = mount.get();
                (!m.meridian_flip_status.is_empty()).then(|| view! {
                    <div class="mount-banner mount-banner--meridian">
                        {tr().mount_meridian_flip}{": "}{m.meridian_flip_status}
                    </div>
                })
            }}
            {move || {
                let m = mount.get();
                (!m.auto_park_countdown.is_empty()).then(|| view! {
                    <div class="mount-banner mount-banner--autopark">
                        {tr().mount_autopark}{": "}{m.auto_park_countdown}
                    </div>
                })
            }}

            // ── Body — responsive grid (CSS @media handles the breakpoint) ──
            <div class="mount-body">

                // ── Left column: Coordinates + GoTo ───────────────────
                <div class="mount-col-left">

                    // Coordinates section
                    <div class="mount-section-title">{move || tr().mount_coords_section}</div>

                    // No-mount placeholder
                    {move || {
                        let m = mount.get();
                        (!m.connected).then(|| view! {
                            <div class="mount-no-device">
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
                                <div class="mount-coord-grid">
                                    {coords.into_iter().map(|(lbl, val)| view! {
                                        <span class="mount-coord-label">
                                            {lbl}{":"}
                                        </span>
                                        <span class="mount-coord-value">
                                            {val}
                                        </span>
                                    }).collect::<Vec<_>>()}
                                </div>
                            }
                        })
                    }}

                    // GoTo section
                    <div class="mount-goto">
                        <div class="mount-section-title">{move || tr().mount_goto_section}</div>
                        <div class="mount-goto-fields">
                            <div>
                                <label class="mount-label">{move || tr().mount_ra_input}</label>
                                <input
                                    class="mount-input"
                                    type="text"
                                    placeholder="HH MM SS"
                                    prop:value=move || ra_input.get()
                                    on:input=move |ev| ra_input.set(event_target_value(&ev))
                                />
                            </div>
                            <div>
                                <label class="mount-label">{move || tr().mount_dec_input}</label>
                                <input
                                    class="mount-input"
                                    type="text"
                                    placeholder="±DD MM SS"
                                    prop:value=move || dec_input.get()
                                    on:input=move |ev| dec_input.set(event_target_value(&ev))
                                />
                            </div>
                            <div>
                                <label class="mount-label">{move || tr().mount_target_input}</label>
                                <input
                                    class="mount-input"
                                    type="text"
                                    placeholder="M42, NGC1234…"
                                    prop:value=move || tgt_input.get()
                                    on:input=move |ev| tgt_input.set(event_target_value(&ev))
                                />
                            </div>
                            <label class="mount-checkbox-row">
                                <input
                                    type="checkbox"
                                    prop:checked=move || j2000.get()
                                    on:change=move |ev| j2000.set(event_target_checked(&ev))
                                />
                                {move || tr().mount_j2000_label}
                            </label>
                            <div class="mount-goto-actions">
                                <button
                                    class="mount-btn mount-btn--ok"
                                    on:click=on_goto.clone()
                                >{move || tr().mount_goto_btn}</button>
                                <button
                                    class="mount-btn mount-btn--info"
                                    on:click=on_sync.clone()
                                >{move || tr().mount_sync_btn}</button>
                            </div>
                            <button
                                class="mount-btn mount-btn--info-2"
                                on:click=on_goto_target.clone()
                            >{move || tr().mount_goto_target_btn}</button>
                        </div>
                    </div>
                </div>

                // ── Right column: D-pad + controls ────────────────────
                <div class="mount-col-right">

                    // D-pad
                    <div>
                        <div class="mount-dpad">
                            // Row 1: empty, N, empty
                            <div></div>
                            <button
                                class="mount-dpad-btn"
                                on:pointerdown=dpad_press("N")
                                on:pointerup=dpad_release("N")
                                on:pointerleave=dpad_release("N")
                            >"↑"</button>
                            <div></div>

                            // Row 2: W, abort, E
                            <button
                                class="mount-dpad-btn"
                                on:pointerdown=dpad_press("W")
                                on:pointerup=dpad_release("W")
                                on:pointerleave=dpad_release("W")
                            >"←"</button>
                            <button
                                class="mount-dpad-btn mount-dpad-btn--abort"
                                on:click=move |_| {
                                    send_abort2(serde_json::json!({"type":"mount_abort","payload":{}}).to_string());
                                }
                                title=move || tr().mount_abort_btn
                            >"●"</button>
                            <button
                                class="mount-dpad-btn"
                                on:pointerdown=dpad_press("E")
                                on:pointerup=dpad_release("E")
                                on:pointerleave=dpad_release("E")
                            >"→"</button>

                            // Row 3: empty, S, empty
                            <div></div>
                            <button
                                class="mount-dpad-btn"
                                on:pointerdown=dpad_press("S")
                                on:pointerup=dpad_release("S")
                                on:pointerleave=dpad_release("S")
                            >"↓"</button>
                            <div></div>
                        </div>
                    </div>

                    // Slew rate selector
                    <div class="mount-slew-rate">
                        <div class="mount-section-title mount-section-title--tight">
                            {move || tr().mount_slew_rate}
                        </div>
                        <div class="mount-slew-rate-row">
                            {(0..8i32).map(|idx| {
                                let lbl = RATE_LABELS[idx as usize];
                                let oc = on_rate(idx);
                                view! {
                                    <button
                                        class="mount-slew-rate-btn"
                                        class:mount-slew-rate-btn--active=move || mount.get().slew_rate == Some(idx)
                                        on:click=oc
                                    >{lbl}</button>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </div>

                    // Action buttons
                    <div class="mount-actions">
                        <button
                            class="mount-btn mount-btn--info"
                            on:click=move |_| {
                                send_park(serde_json::json!({"type":"mount_park","payload":{}}).to_string());
                            }
                        >{move || tr().mount_park_btn}</button>
                        <button
                            class="mount-btn mount-btn--success"
                            on:click=move |_| {
                                send_unpark(serde_json::json!({"type":"mount_unpark","payload":{}}).to_string());
                            }
                        >{move || tr().mount_unpark_btn}</button>
                        <button
                            class="mount-btn mount-btn--danger"
                            on:click=move |_| {
                                send_abort(serde_json::json!({"type":"mount_abort","payload":{}}).to_string());
                            }
                        >{move || tr().mount_abort_btn}</button>
                        <button
                            class="mount-btn"
                            class:mount-btn--track-on=move || mount.get().tracking
                            class:mount-btn--track-off=move || !mount.get().tracking
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
