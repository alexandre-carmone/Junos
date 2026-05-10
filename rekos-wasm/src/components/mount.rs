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

// Shared button class fragment: .btn primitive, full-width, slightly taller
// touch target. Individual call sites add a state colour modifier.
const MOUNT_BTN: &str = "btn btn--block min-h-[40px]";
const SECTION_TITLE: &str = "font-ui font-semibold text-sm text-text-blue tracking-[0.1em] border-b border-border-base pb-sp-1 mb-sp-3";
const DPAD_BTN: &str = "btn-icon btn-icon--lg !rounded-lg border-border-accent-2 text-text-blue text-[18px] font-bold leading-none touch-none select-none";

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
    let send_flip    = mk_send!();

    // ── Auto-flip settings (round-tripped via mount_set_all_settings) ─
    // Local edit signals; kept in sync with the snapshot via Effects so
    // they pick up KStars-side changes without freezing user input.
    let flip_enabled = RwSignal::new(false);
    let flip_offset  = RwSignal::new(0.0_f64);
    Effect::new(move |_| {
        if let Some(b) = mount.get().meridian_flip_enabled {
            flip_enabled.set(b);
        }
    });
    Effect::new(move |_| {
        if let Some(v) = mount.get().meridian_flip_offset_deg {
            flip_offset.set(v);
        }
    });

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
            class="absolute inset-0 bg-bg text-text font-mono overflow-y-auto overflow-x-hidden pb-[60px]"
            on:click=|ev: MouseEvent| ev.stop_propagation()
        >
            // ── Header ────────────────────────────────────────────────
            <div class="flex items-center gap-sp-3 pt-sp-3 pr-sp-4 pb-[6px] pl-sp-4 border-b border-border-base flex-wrap">
                <span class="font-mono font-bold text-md text-text-dim tracking-[0.08em]">
                    {move || tr().mount_title}
                </span>
                {move || {
                    let m = mount.get();
                    m.device_name.map(|dev| view! {
                        <span class="font-mono text-sm text-[#666] ml-sp-1">{dev}</span>
                    })
                }}
                <div class="ml-auto">
                    {move || {
                        let m = mount.get();
                        let (label, color) = status_label_color(&m, tr());
                        view! {
                            <span
                                class="px-sp-2 py-[3px] rounded-sm border border-current bg-black/50 font-mono font-bold text-sm tracking-[0.08em]"
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
                    <div class="px-sp-4 py-sp-1 font-ui text-sm bg-state-warn/10 border-b border-state-warn/40 text-state-warn">
                        {tr().mount_meridian_flip}{": "}{m.meridian_flip_status}
                    </div>
                })
            }}
            {move || {
                let m = mount.get();
                (!m.auto_park_countdown.is_empty()).then(|| view! {
                    <div class="px-sp-4 py-sp-1 font-ui text-sm bg-state-info/10 border-b border-state-info/40 text-state-info">
                        {tr().mount_autopark}{": "}{m.auto_park_countdown}
                    </div>
                })
            }}

            // ── Body — single column on phones, 2-col grid ≥901 px ────
            <div class="flex flex-col min-[901px]:grid min-[901px]:grid-cols-2 min-[901px]:gap-0 min-[901px]:h-[calc(100%-80px)]">

                // ── Left column: Coordinates + GoTo ───────────────────
                <div class="p-sp-4 overflow-y-auto max-[900px]:border-b min-[901px]:border-r border-[#1a1a2e]">

                    // Coordinates section
                    <div class=SECTION_TITLE>{move || tr().mount_coords_section}</div>

                    // No-mount placeholder
                    {move || {
                        let m = mount.get();
                        (!m.connected).then(|| view! {
                            <div class="text-[#555] font-mono text-[12px] py-5 text-center">
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
                                <div class="grid grid-cols-[auto_1fr] gap-y-sp-1 gap-x-3 items-baseline">
                                    {coords.into_iter().map(|(lbl, val)| view! {
                                        <span class="font-mono text-sm text-text-faint whitespace-nowrap">
                                            {lbl}{":"}
                                        </span>
                                        <span class="font-mono text-[12px] text-text-dim tabular-nums">
                                            {val}
                                        </span>
                                    }).collect::<Vec<_>>()}
                                </div>
                            }
                        })
                    }}

                    // In-tab meridian flip status chip — duplicates the top
                    // banner but stays visible while scrolling the body.
                    {move || {
                        let m = mount.get();
                        (!m.meridian_flip_status.is_empty()).then(|| view! {
                            <div class="mt-sp-2 inline-flex items-center gap-sp-2 px-sp-2 py-[3px] rounded-sm border border-state-warn/40 bg-state-warn/15 text-state-warn font-mono text-sm">
                                <span class="font-bold tracking-[0.08em]">{tr().mount_meridian_flip}{":"}</span>
                                <span>{m.meridian_flip_status}</span>
                            </div>
                        })
                    }}

                    // GoTo section
                    <div class="mt-sp-5">
                        <div class=SECTION_TITLE>{move || tr().mount_goto_section}</div>
                        <div class="flex flex-col gap-sp-2">
                            <div>
                                <label class="font-mono text-sm text-text-faint mb-[2px] block">{move || tr().mount_ra_input}</label>
                                <input
                                    class="input w-full font-mono"
                                    type="text"
                                    placeholder="HH MM SS"
                                    prop:value=move || ra_input.get()
                                    on:input=move |ev| ra_input.set(event_target_value(&ev))
                                />
                            </div>
                            <div>
                                <label class="font-mono text-sm text-text-faint mb-[2px] block">{move || tr().mount_dec_input}</label>
                                <input
                                    class="input w-full font-mono"
                                    type="text"
                                    placeholder="±DD MM SS"
                                    prop:value=move || dec_input.get()
                                    on:input=move |ev| dec_input.set(event_target_value(&ev))
                                />
                            </div>
                            <div>
                                <label class="font-mono text-sm text-text-faint mb-[2px] block">{move || tr().mount_target_input}</label>
                                <input
                                    class="input w-full font-mono"
                                    type="text"
                                    placeholder="M42, NGC1234…"
                                    prop:value=move || tgt_input.get()
                                    on:input=move |ev| tgt_input.set(event_target_value(&ev))
                                />
                            </div>
                            <label class="flex items-center gap-[6px] font-mono text-[12px] text-text-muted cursor-pointer">
                                <input
                                    type="checkbox"
                                    prop:checked=move || j2000.get()
                                    on:change=move |ev| j2000.set(event_target_checked(&ev))
                                />
                                {move || tr().mount_j2000_label}
                            </label>
                            <div class="grid grid-cols-2 gap-[6px]">
                                <button
                                    class=format!("{MOUNT_BTN} btn-primary")
                                    on:click=on_goto.clone()
                                >{move || tr().mount_goto_btn}</button>
                                <button
                                    class=format!("{MOUNT_BTN} text-text-blue")
                                    on:click=on_sync.clone()
                                >{move || tr().mount_sync_btn}</button>
                            </div>
                            <button
                                class=format!("{MOUNT_BTN} text-state-info")
                                on:click=on_goto_target.clone()
                            >{move || tr().mount_goto_target_btn}</button>
                        </div>
                    </div>
                </div>

                // ── Right column: D-pad + controls ────────────────────
                <div class="p-sp-4 flex flex-col gap-4 items-center">

                    // D-pad
                    <div>
                        <div class="grid grid-cols-[repeat(3,48px)] grid-rows-[repeat(3,48px)] max-md:grid-cols-[repeat(3,56px)] max-md:grid-rows-[repeat(3,56px)] gap-[6px]">
                            // Row 1: empty, N, empty
                            <div></div>
                            <button
                                class=DPAD_BTN
                                on:pointerdown=dpad_press("N")
                                on:pointerup=dpad_release("N")
                                on:pointerleave=dpad_release("N")
                            >"↑"</button>
                            <div></div>

                            // Row 2: W, abort, E
                            <button
                                class=DPAD_BTN
                                on:pointerdown=dpad_press("W")
                                on:pointerup=dpad_release("W")
                                on:pointerleave=dpad_release("W")
                            >"←"</button>
                            <button
                                class="btn-icon btn-icon--lg btn-icon--circle btn-danger text-[14px] font-bold leading-none select-none"
                                on:click=move |_| {
                                    send_abort2(serde_json::json!({"type":"mount_abort","payload":{}}).to_string());
                                }
                                title=move || tr().mount_abort_btn
                            >"●"</button>
                            <button
                                class=DPAD_BTN
                                on:pointerdown=dpad_press("E")
                                on:pointerup=dpad_release("E")
                                on:pointerleave=dpad_release("E")
                            >"→"</button>

                            // Row 3: empty, S, empty
                            <div></div>
                            <button
                                class=DPAD_BTN
                                on:pointerdown=dpad_press("S")
                                on:pointerup=dpad_release("S")
                                on:pointerleave=dpad_release("S")
                            >"↓"</button>
                            <div></div>
                        </div>
                    </div>

                    // Slew rate selector
                    <div class="w-full max-w-[320px]">
                        <div class="font-mono font-bold text-sm text-text-blue tracking-[0.1em] border-b border-[#223] pb-sp-1 mb-[6px]">
                            {move || tr().mount_slew_rate}
                        </div>
                        <div class="flex gap-sp-1 flex-wrap justify-center">
                            {(0..8i32).map(|idx| {
                                let lbl = RATE_LABELS[idx as usize];
                                let oc = on_rate(idx);
                                view! {
                                    <button
                                        class=move || {
                                            let base = "btn btn--sm font-mono min-w-[36px]";
                                            if mount.get().slew_rate == Some(idx) {
                                                format!("{base} btn--active")
                                            } else {
                                                format!("{base} btn-ghost")
                                            }
                                        }
                                        on:click=oc
                                    >{lbl}</button>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </div>

                    // Meridian flip auto-config — round-trips via
                    // mount_set_all_settings; keys mirror the KStars
                    // QObjects (mount.ui:111, 124).
                    <div class="w-full max-w-[320px]">
                        <div class=SECTION_TITLE>{move || tr().mount_meridian_flip}</div>
                        <div class="flex flex-col gap-sp-2">
                            <label class="flex items-center gap-sp-2 font-mono text-sm text-text-muted cursor-pointer">
                                <input
                                    type="checkbox"
                                    prop:checked=move || flip_enabled.get()
                                    on:change=move |ev| flip_enabled.set(event_target_checked(&ev))
                                />
                                {move || tr().mount_auto_flip}
                            </label>
                            <div>
                                <label class="font-mono text-sm text-text-faint mb-[2px] block">
                                    {move || tr().mount_past_meridian}{" (°)"}
                                </label>
                                <input
                                    class="input w-full font-mono"
                                    type="number"
                                    step="0.1"
                                    prop:value=move || format!("{:.2}", flip_offset.get())
                                    on:input=move |ev| {
                                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                            flip_offset.set(v);
                                        }
                                    }
                                />
                            </div>
                            <button
                                class=format!("{MOUNT_BTN} btn-primary")
                                on:click=move |_| {
                                    send_flip(serde_json::json!({
                                        "type": "mount_set_all_settings",
                                        "payload": {
                                            "executeMeridianFlip":      flip_enabled.get_untracked(),
                                            "meridianFlipOffsetDegrees": flip_offset.get_untracked(),
                                        }
                                    }).to_string());
                                }
                            >{move || tr().save_btn}</button>
                        </div>
                    </div>

                    // Action buttons
                    <div class="w-full max-w-[320px] grid grid-cols-2 gap-sp-2">
                        <button
                            class=format!("{MOUNT_BTN} text-text-blue")
                            on:click=move |_| {
                                send_park(serde_json::json!({"type":"mount_park","payload":{}}).to_string());
                            }
                        >{move || tr().mount_park_btn}</button>
                        <button
                            class=format!("{MOUNT_BTN} text-state-ok")
                            on:click=move |_| {
                                send_unpark(serde_json::json!({"type":"mount_unpark","payload":{}}).to_string());
                            }
                        >{move || tr().mount_unpark_btn}</button>
                        <button
                            class=format!("{MOUNT_BTN} btn-danger")
                            on:click=move |_| {
                                send_abort(serde_json::json!({"type":"mount_abort","payload":{}}).to_string());
                            }
                        >{move || tr().mount_abort_btn}</button>
                        <button
                            class=move || if mount.get().tracking {
                                format!("{MOUNT_BTN} btn--active text-state-ok")
                            } else {
                                format!("{MOUNT_BTN} text-text-faint")
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
        return (tr.disconnected, "var(--text-faint)");
    }
    if m.slewing {
        return (tr.mount_status_slewing, "var(--state-warn)");
    }
    if m.tracking {
        return (tr.mount_status_tracking, "var(--state-ok)");
    }
    if m.parked {
        return (tr.mount_status_parked, "var(--text-blue)");
    }
    // Check raw status string for parking
    let s = m.status_str.to_lowercase();
    if s.contains("parking") {
        return (tr.mount_status_parking, "var(--text-blue)");
    }
    if s.contains("error") {
        return (tr.mount_status_error, "var(--state-err)");
    }
    (tr.mount_status_idle, "var(--text-muted)")
}
