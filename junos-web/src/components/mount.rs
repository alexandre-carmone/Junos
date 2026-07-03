use std::sync::Arc;

use leptos::prelude::*;
use leptos::web_sys;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::MouseEvent;

use crate::compat::{FilterWheelSnapshot, MountSnapshot, SolveSnapshot};
use crate::components::coord_input::{CoordInput, CoordMode};
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
pub fn MountTab(
    mount: Signal<MountSnapshot>,
    solve: Signal<SolveSnapshot>,
    align_settings: RwSignal<serde_json::Value>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    send: SendCmd,
) -> impl IntoView {
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

    // ── Plate-solve UI state ─────────────────────────────────────────
    // Overlay open + "params dirty" edit signals seeded from align_settings.
    let settings_open = RwSignal::new(false);
    // Plate-solve process overlay (live timeline + solver log).
    let log_open = RwSignal::new(false);

    // Edit signals — seeded lazily when overlay opens, updated by inputs.
    let edit_exposure       = RwSignal::new(1.0_f64);
    let edit_accuracy       = RwSignal::new(30.0_f64);
    let edit_settling       = RwSignal::new(500.0_f64);
    let edit_binning        = RwSignal::new("1x1".to_string());
    let edit_filter         = RwSignal::new(String::new());
    let edit_iso            = RwSignal::new(String::new());
    let edit_gain           = RwSignal::new(100_f64);
    let edit_dark_frame     = RwSignal::new(false);
    let edit_post_action    = RwSignal::new("nothing".to_string()); // "sync"|"slew"|"nothing"
    let edit_solver_local   = RwSignal::new(true);                   // false → remote
    let edit_use_scale      = RwSignal::new(true);
    let edit_use_position   = RwSignal::new(true);
    let edit_rotator_thr    = RwSignal::new(60_i64);
    let edit_rotator_ctrl   = RwSignal::new(false);

    // Seed edit signals from latest align_settings each time overlay opens.
    Effect::new(move |_| {
        if !settings_open.get() {
            return;
        }
        let s = align_settings.get();
        if let Some(v) = s.get("alignExposure").and_then(|x| x.as_f64()) {
            edit_exposure.set(v);
        }
        if let Some(v) = s.get("alignAccuracyThreshold").and_then(|x| x.as_f64()) {
            edit_accuracy.set(v);
        }
        if let Some(v) = s.get("alignSettlingTime").and_then(|x| x.as_f64()) {
            edit_settling.set(v);
        }
        if let Some(v) = s.get("alignBinning").and_then(|x| x.as_str()) {
            edit_binning.set(v.to_string());
        }
        if let Some(v) = s.get("alignFilter").and_then(|x| x.as_str()) {
            edit_filter.set(v.to_string());
        }
        if let Some(v) = s.get("alignISO").and_then(|x| x.as_str()) {
            edit_iso.set(v.to_string());
        }
        if let Some(v) = s.get("alignGain").and_then(|x| x.as_f64()) {
            edit_gain.set(v);
        }
        if let Some(v) = s.get("alignDarkFrame").and_then(|x| x.as_bool()) {
            edit_dark_frame.set(v);
        }
        if s.get("syncR").and_then(|x| x.as_bool()).unwrap_or(false) {
            edit_post_action.set("sync".into());
        } else if s.get("slewR").and_then(|x| x.as_bool()).unwrap_or(false) {
            edit_post_action.set("slew".into());
        } else if s.get("nothingR").and_then(|x| x.as_bool()).unwrap_or(false) {
            edit_post_action.set("nothing".into());
        }
        if let Some(v) = s.get("remoteSolverR").and_then(|x| x.as_bool()) {
            edit_solver_local.set(!v);
        } else if let Some(v) = s.get("localSolverR").and_then(|x| x.as_bool()) {
            edit_solver_local.set(v);
        }
    });

    // Escape closes the overlay (mirrors focus.rs:108-121).
    {
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            move |e: web_sys::KeyboardEvent| {
                if e.key() == "Escape" {
                    if settings_open.get_untracked() {
                        settings_open.set(false);
                    }
                    if log_open.get_untracked() {
                        log_open.set(false);
                    }
                }
            },
        );
        if let Some(win) = web_sys::window() {
            let _ = win.add_event_listener_with_callback(
                "keydown",
                cb.as_ref().unchecked_ref(),
            );
        }
        cb.forget();
    }

    // Handlers
    let send_solve = mk_send!();
    let on_capture_solve = move |_: MouseEvent| {
        send_solve(serde_json::json!({"type":"align_solve","payload":{}}).to_string());
    };
    let send_align_stop = mk_send!();
    let on_align_stop = move |_: MouseEvent| {
        send_align_stop(serde_json::json!({"type":"align_stop","payload":{}}).to_string());
    };

    // Hidden file input ref + Load FITS button trigger.
    let file_input_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let on_load_fits_click = move |_: MouseEvent| {
        if let Some(el) = file_input_ref.get() {
            el.click();
        }
    };
    let send_load_fits = mk_send!();
    let on_file_change = move |ev: web_sys::Event| {
        let target = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok());
        let files = match target.as_ref().and_then(|i| i.files()) {
            Some(f) => f,
            None => return,
        };
        let file = match files.get(0) {
            Some(f) => f,
            None => return,
        };
        let name = file.name();
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_else(|| "fits".into());
        let reader = match web_sys::FileReader::new() {
            Ok(r) => r,
            Err(_) => return,
        };
        let reader_cl = reader.clone();
        let send_for_cb = Arc::clone(&send_load_fits);
        let onload = Closure::<dyn FnMut()>::new(move || {
            let buf = match reader_cl.result() {
                Ok(b) => b,
                Err(_) => return,
            };
            let array = web_sys::js_sys::Uint8Array::new(&buf);
            // Build a binary string (each byte = one char code) then btoa.
            let len = array.length() as usize;
            let mut bytes = vec![0u8; len];
            array.copy_to(&mut bytes);
            let bin: String = bytes.iter().map(|&b| b as char).collect();
            let win = match web_sys::window() {
                Some(w) => w,
                None => return,
            };
            let b64 = match win.btoa(&bin) {
                Ok(s) => s,
                Err(_) => return,
            };
            send_for_cb(serde_json::json!({
                "type": "align_load_and_slew",
                "payload": { "data": b64, "ext": ext }
            }).to_string());
        });
        let _ = reader.set_onload(Some(onload.as_ref().unchecked_ref()));
        onload.forget();
        let _ = reader.read_as_array_buffer(&file);
        // Reset value so picking the same file again re-triggers change.
        if let Some(t) = target {
            t.set_value("");
        }
    };

    // Apply overlay → align_set_all_settings + align_set_astrometry_settings.
    // Stored in an Arc so the <Show>-body closure (Fn) can clone+invoke it.
    let send_apply_settings   = mk_send!();
    let send_apply_astrometry = mk_send!();
    let on_apply: Arc<dyn Fn(MouseEvent) + Send + Sync + 'static> = Arc::new(move |_: MouseEvent| {
        let mut map = align_settings
            .get_untracked()
            .as_object()
            .cloned()
            .unwrap_or_default();
        map.insert("alignExposure".into(), serde_json::json!(edit_exposure.get_untracked()));
        map.insert("alignAccuracyThreshold".into(), serde_json::json!(edit_accuracy.get_untracked()));
        map.insert("alignSettlingTime".into(), serde_json::json!(edit_settling.get_untracked()));
        map.insert("alignBinning".into(), serde_json::json!(edit_binning.get_untracked()));
        let f = edit_filter.get_untracked();
        if !f.is_empty() {
            map.insert("alignFilter".into(), serde_json::json!(f));
        }
        let iso = edit_iso.get_untracked();
        if !iso.is_empty() {
            map.insert("alignISO".into(), serde_json::json!(iso));
        }
        map.insert("alignGain".into(), serde_json::json!(edit_gain.get_untracked()));
        map.insert("alignDarkFrame".into(), serde_json::json!(edit_dark_frame.get_untracked()));
        let pa = edit_post_action.get_untracked();
        map.insert("syncR".into(), serde_json::json!(pa == "sync"));
        map.insert("slewR".into(), serde_json::json!(pa == "slew"));
        map.insert("nothingR".into(), serde_json::json!(pa == "nothing"));
        let local = edit_solver_local.get_untracked();
        map.insert("localSolverR".into(), serde_json::json!(local));
        map.insert("remoteSolverR".into(), serde_json::json!(!local));

        send_apply_settings(serde_json::json!({
            "type": "align_set_all_settings",
            "payload": serde_json::Value::Object(map),
        }).to_string());

        send_apply_astrometry(serde_json::json!({
            "type": "align_set_astrometry_settings",
            "payload": {
                "scale":          edit_use_scale.get_untracked(),
                "position":       edit_use_position.get_untracked(),
                "threshold":      edit_rotator_thr.get_untracked(),
                "rotator_control": edit_rotator_ctrl.get_untracked(),
            }
        }).to_string());
        settings_open.set(false);
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
                                <CoordInput mode=CoordMode::Hms value=ra_input aria_label="RA" />
                            </div>
                            <div>
                                <label class="font-mono text-sm text-text-faint mb-[2px] block">{move || tr().mount_dec_input}</label>
                                <CoordInput mode=CoordMode::DmsSigned value=dec_input aria_label="Dec" />
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

                    // ── Solve section ─────────────────────────────────
                    <div class="w-full max-w-[320px]">
                        <div class=SECTION_TITLE>{move || tr().mount_solve_section}</div>

                        // Status row
                        <div class="grid grid-cols-[auto_1fr] gap-y-sp-1 gap-x-3 items-baseline mb-sp-2">
                            <span class="font-mono text-sm text-text-faint whitespace-nowrap">
                                {move || tr().mount_solve_status}{":"}
                            </span>
                            <span class="font-mono text-[12px] text-text-dim">
                                {move || solve.get().status.unwrap_or_else(|| "—".into())}
                            </span>
                        </div>

                        // Solution rows or "no solution"
                        {move || {
                            let s = solve.get();
                            let has = s.ra_jnow_deg.is_some() || s.dec_jnow_deg.is_some();
                            if !has {
                                return view! {
                                    <div class="text-[#555] font-mono text-[12px] py-sp-2 text-center">
                                        {tr().mount_solve_no_solution}
                                    </div>
                                }.into_any();
                            }
                            let rows: Vec<(&'static str, String)> = vec![
                                (tr().mount_ra_jnow,
                                 s.ra_jnow_deg.map(|d| fmt_hms(d / 15.0)).unwrap_or_else(|| "—".into())),
                                (tr().mount_dec_jnow,
                                 s.dec_jnow_deg.map(fmt_dms).unwrap_or_else(|| "—".into())),
                                (tr().mount_solve_pa,
                                 s.rotation_deg.map(|v| format!("{v:.2}°")).unwrap_or_else(|| "—".into())),
                                (tr().mount_solve_pixscale,
                                 s.pixscale_arcsec.map(|v| format!("{v:.2}\"/px")).unwrap_or_else(|| "—".into())),
                                (tr().mount_solve_solved_at,
                                 s.solved_at_ms.map(|t| {
                                     let now = web_sys::js_sys::Date::now();
                                     let dt = ((now - t) / 1000.0).max(0.0) as u64;
                                     if dt < 60 { format!("{dt} s") }
                                     else if dt < 3600 { format!("{} min", dt / 60) }
                                     else { format!("{} h", dt / 3600) }
                                 }).unwrap_or_else(|| "—".into())),
                            ];
                            view! {
                                <div class="grid grid-cols-[auto_1fr] gap-y-sp-1 gap-x-3 items-baseline">
                                    {rows.into_iter().map(|(lbl, val)| view! {
                                        <span class="font-mono text-sm text-text-faint whitespace-nowrap">
                                            {lbl}{":"}
                                        </span>
                                        <span class="font-mono text-[12px] text-text-dim tabular-nums">
                                            {val}
                                        </span>
                                    }).collect::<Vec<_>>()}
                                </div>
                            }.into_any()
                        }}

                        // Action buttons + hidden file input
                        <div class="grid grid-cols-2 gap-sp-2 mt-sp-2">
                            <button
                                class=format!("{MOUNT_BTN} btn-primary")
                                on:click=on_capture_solve
                            >{move || tr().mount_solve_capture}</button>
                            <button
                                class=format!("{MOUNT_BTN} btn-danger")
                                on:click=on_align_stop
                            >{move || tr().mount_solve_stop}</button>
                            <button
                                class=format!("{MOUNT_BTN} text-state-info")
                                on:click=on_load_fits_click
                            >{move || tr().mount_solve_load_fits}</button>
                            <button
                                class=format!("{MOUNT_BTN} text-text-blue")
                                on:click=move |_| settings_open.set(true)
                            >{move || tr().mount_solve_params}</button>
                        </div>
                        <input
                            node_ref=file_input_ref
                            type="file"
                            accept=".fits,.fit,.fts"
                            style="display:none"
                            on:change=on_file_change
                        />

                        // Process / log overlay opener — pulses while solving.
                        <button
                            class=move || {
                                let base = format!("{MOUNT_BTN} mt-sp-2 flex items-center justify-center gap-sp-2");
                                let live = solve.get().status
                                    .map(|s| align_in_progress(&s)).unwrap_or(false);
                                if live { format!("{base} text-state-info animate-pulse") }
                                else { format!("{base} text-text-blue") }
                            }
                            on:click=move |_| log_open.set(true)
                        >
                            <span>{move || tr().mount_solve_process}</span>
                            {move || {
                                let live = solve.get().status
                                    .map(|s| align_in_progress(&s)).unwrap_or(false);
                                live.then(|| view! {
                                    <span class="w-[7px] h-[7px] rounded-full bg-state-info" />
                                })
                            }}
                        </button>
                    </div>
                </div>
            </div>

            // ── Plate-solve parameters overlay ────────────────────────
            <Show when=move || settings_open.get()>
                <div
                    class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2"
                    on:click=move |_| settings_open.set(false)
                >
                    <div
                        class="w-full max-w-[980px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col"
                        on:click=|ev: MouseEvent| ev.stop_propagation()
                    >
                        // Header
                        <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                            <h2 class="text-text-blue text-sm uppercase tracking-[0.08em] m-0">
                                {move || tr().mount_solve_params}
                            </h2>
                            <button
                                class="btn btn-ghost"
                                on:click=move |_| settings_open.set(false)
                            >{move || tr().imaging_close}</button>
                        </div>

                        // Body — three groups
                        <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 grid grid-cols-[repeat(auto-fit,minmax(280px,1fr))] gap-sp-4 max-[759px]:p-sp-3">

                            // Capture group
                            <fieldset class="border border-border-base rounded-[4px] p-sp-3 m-0">
                                <legend class="text-text-blue text-sm uppercase tracking-[0.08em] px-sp-2">
                                    {move || tr().mount_solve_params_capture}
                                </legend>
                                <div class="flex flex-col gap-sp-2">
                                    <label class="font-mono text-sm text-text-faint">
                                        {move || tr().align_exposure}
                                        <input class="input w-full font-mono mt-[2px]" type="number" step="0.1" min="0"
                                            prop:value=move || format!("{:.2}", edit_exposure.get())
                                            on:input=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<f64>() { edit_exposure.set(v); } } />
                                    </label>
                                    <label class="font-mono text-sm text-text-faint">
                                        {move || tr().mount_solve_binning}
                                        <input class="input w-full font-mono mt-[2px]" type="text"
                                            prop:value=move || edit_binning.get()
                                            on:input=move |ev| edit_binning.set(event_target_value(&ev)) />
                                    </label>
                                    <label class="font-mono text-sm text-text-faint">
                                        {move || tr().mount_solve_filter}
                                        <select class="input w-full font-mono mt-[2px]"
                                            prop:value=move || edit_filter.get()
                                            on:change=move |ev| edit_filter.set(event_target_value(&ev))
                                        >
                                            <option value="">"—"</option>
                                            {move || {
                                                let names = filter_wheel.get().filter_names;
                                                let current = edit_filter.get();
                                                let mut all = names.clone();
                                                if !current.is_empty() && !names.contains(&current) {
                                                    all.push(current.clone());
                                                }
                                                all.into_iter().map(|n| {
                                                    let sel = current == n;
                                                    let label = n.clone();
                                                    view! {
                                                        <option value=n selected=move || sel>{label}</option>
                                                    }
                                                }).collect::<Vec<_>>()
                                            }}
                                        </select>
                                    </label>
                                    <label class="font-mono text-sm text-text-faint">
                                        {move || tr().mount_solve_iso}
                                        <input class="input w-full font-mono mt-[2px]" type="text"
                                            prop:value=move || edit_iso.get()
                                            on:input=move |ev| edit_iso.set(event_target_value(&ev)) />
                                    </label>
                                    <label class="font-mono text-sm text-text-faint">
                                        {move || tr().mount_solve_gain}
                                        <input class="input w-full font-mono mt-[2px]" type="number" step="1"
                                            prop:value=move || format!("{:.0}", edit_gain.get())
                                            on:input=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<f64>() { edit_gain.set(v); } } />
                                    </label>
                                    <label class="flex items-center gap-sp-2 font-mono text-sm text-text-muted cursor-pointer">
                                        <input type="checkbox"
                                            prop:checked=move || edit_dark_frame.get()
                                            on:change=move |ev| edit_dark_frame.set(event_target_checked(&ev)) />
                                        {move || tr().mount_solve_dark_frame}
                                    </label>
                                </div>
                            </fieldset>

                            // Solver group
                            <fieldset class="border border-border-base rounded-[4px] p-sp-3 m-0">
                                <legend class="text-text-blue text-sm uppercase tracking-[0.08em] px-sp-2">
                                    {move || tr().mount_solve_params_solver}
                                </legend>
                                <div class="flex flex-col gap-sp-2">
                                    <label class="font-mono text-sm text-text-faint">
                                        {move || tr().align_accuracy}
                                        <input class="input w-full font-mono mt-[2px]" type="number" step="1" min="0"
                                            prop:value=move || format!("{:.0}", edit_accuracy.get())
                                            on:input=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<f64>() { edit_accuracy.set(v); } } />
                                    </label>
                                    <label class="font-mono text-sm text-text-faint">
                                        {move || tr().mount_solve_settling_ms}
                                        <input class="input w-full font-mono mt-[2px]" type="number" step="50" min="0"
                                            prop:value=move || format!("{:.0}", edit_settling.get())
                                            on:input=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<f64>() { edit_settling.set(v); } } />
                                    </label>
                                    <div class="font-mono text-sm text-text-faint mt-sp-1">
                                        {move || tr().mount_solve_post_action}
                                    </div>
                                    <div class="flex flex-col gap-[2px] pl-sp-2">
                                        <label class="flex items-center gap-sp-2 font-mono text-sm cursor-pointer">
                                            <input type="radio" name="mount-post-action"
                                                prop:checked=move || edit_post_action.get() == "sync"
                                                on:change=move |_| edit_post_action.set("sync".into()) />
                                            {move || tr().mount_solve_post_sync}
                                        </label>
                                        <label class="flex items-center gap-sp-2 font-mono text-sm cursor-pointer">
                                            <input type="radio" name="mount-post-action"
                                                prop:checked=move || edit_post_action.get() == "slew"
                                                on:change=move |_| edit_post_action.set("slew".into()) />
                                            {move || tr().mount_solve_post_slew}
                                        </label>
                                        <label class="flex items-center gap-sp-2 font-mono text-sm cursor-pointer">
                                            <input type="radio" name="mount-post-action"
                                                prop:checked=move || edit_post_action.get() == "nothing"
                                                on:change=move |_| edit_post_action.set("nothing".into()) />
                                            {move || tr().mount_solve_post_nothing}
                                        </label>
                                    </div>
                                    <div class="font-mono text-sm text-text-faint mt-sp-1">
                                        {move || tr().mount_solve_solver_source}
                                    </div>
                                    <div class="flex flex-col gap-[2px] pl-sp-2">
                                        <label class="flex items-center gap-sp-2 font-mono text-sm cursor-pointer">
                                            <input type="radio" name="mount-solver-source"
                                                prop:checked=move || edit_solver_local.get()
                                                on:change=move |_| edit_solver_local.set(true) />
                                            {move || tr().mount_solve_solver_local}
                                        </label>
                                        <label class="flex items-center gap-sp-2 font-mono text-sm cursor-pointer">
                                            <input type="radio" name="mount-solver-source"
                                                prop:checked=move || !edit_solver_local.get()
                                                on:change=move |_| edit_solver_local.set(false) />
                                            {move || tr().mount_solve_solver_remote}
                                        </label>
                                    </div>
                                </div>
                            </fieldset>

                            // Astrometry hints group
                            <fieldset class="border border-border-base rounded-[4px] p-sp-3 m-0">
                                <legend class="text-text-blue text-sm uppercase tracking-[0.08em] px-sp-2">
                                    {move || tr().mount_solve_params_astrometry}
                                </legend>
                                <div class="flex flex-col gap-sp-2">
                                    <label class="flex items-center gap-sp-2 font-mono text-sm text-text-muted cursor-pointer">
                                        <input type="checkbox"
                                            prop:checked=move || edit_use_scale.get()
                                            on:change=move |ev| edit_use_scale.set(event_target_checked(&ev)) />
                                        {move || tr().mount_solve_use_scale}
                                    </label>
                                    <label class="flex items-center gap-sp-2 font-mono text-sm text-text-muted cursor-pointer">
                                        <input type="checkbox"
                                            prop:checked=move || edit_use_position.get()
                                            on:change=move |ev| edit_use_position.set(event_target_checked(&ev)) />
                                        {move || tr().mount_solve_use_position}
                                    </label>
                                    <label class="font-mono text-sm text-text-faint">
                                        {move || tr().mount_solve_rotator_threshold}
                                        <input class="input w-full font-mono mt-[2px]" type="number" step="1" min="0"
                                            prop:value=move || format!("{}", edit_rotator_thr.get())
                                            on:input=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<i64>() { edit_rotator_thr.set(v); } } />
                                    </label>
                                    <label class="flex items-center gap-sp-2 font-mono text-sm text-text-muted cursor-pointer">
                                        <input type="checkbox"
                                            prop:checked=move || edit_rotator_ctrl.get()
                                            on:change=move |ev| edit_rotator_ctrl.set(event_target_checked(&ev)) />
                                        {move || tr().mount_solve_rotator_control}
                                    </label>
                                </div>
                            </fieldset>
                        </div>

                        // Footer
                        <div class="flex justify-end gap-sp-2 py-sp-3 px-sp-4 border-t border-border-base bg-[rgba(10,12,20,0.8)]">
                            <button class="btn btn-ghost"
                                on:click=move |_| settings_open.set(false)
                            >{move || tr().cancel}</button>
                            <button class=format!("{MOUNT_BTN} btn-primary")
                                on:click={ let oa = Arc::clone(&on_apply); move |ev| oa(ev) }
                            >{move || tr().mount_solve_apply}</button>
                        </div>
                    </div>
                </div>
            </Show>

            // ── Plate-solve process overlay (live timeline + solver log) ──
            <Show when=move || log_open.get()>
                <div
                    class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2"
                    on:click=move |_| log_open.set(false)
                >
                    <div
                        class="w-full max-w-[820px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col"
                        on:click=|ev: MouseEvent| ev.stop_propagation()
                    >
                        // Header — title + live status chip
                        <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                            <div class="flex items-center gap-sp-3">
                                <h2 class="text-text-blue text-sm uppercase tracking-[0.08em] m-0">
                                    {move || tr().mount_solve_process}
                                </h2>
                                {move || {
                                    let s = solve.get().status.unwrap_or_default();
                                    if s.is_empty() { return ().into_any(); }
                                    let cls = format!(
                                        "font-mono text-[12px] px-sp-2 py-[2px] rounded-sm border border-border-base {}",
                                        align_status_class(&s),
                                    );
                                    view! { <span class=cls>{s}</span> }.into_any()
                                }}
                            </div>
                            <button
                                class="btn btn-ghost"
                                on:click=move |_| log_open.set(false)
                            >{move || tr().imaging_close}</button>
                        </div>

                        // Body
                        <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 flex flex-col gap-sp-4 max-[759px]:p-sp-3">

                            // Download progress (remote solver only)
                            {move || {
                                solve.get().download_progress.map(|dp| view! {
                                    <div class="font-mono text-[12px] text-state-info bg-state-info/10 border border-state-info/40 rounded-sm px-sp-3 py-sp-2">
                                        {tr().mount_solve_download}{": "}{dp}
                                    </div>
                                })
                            }}

                            // Solution summary (reuses same fields as the inline readout)
                            {move || {
                                let s = solve.get();
                                let has = s.ra_jnow_deg.is_some() || s.dec_jnow_deg.is_some();
                                if !has { return ().into_any(); }
                                let rows: Vec<(&'static str, String)> = vec![
                                    (tr().mount_ra_jnow,
                                     s.ra_jnow_deg.map(|d| fmt_hms(d / 15.0)).unwrap_or_else(|| "—".into())),
                                    (tr().mount_dec_jnow,
                                     s.dec_jnow_deg.map(fmt_dms).unwrap_or_else(|| "—".into())),
                                    (tr().mount_solve_pa,
                                     s.rotation_deg.map(|v| format!("{v:.2}°")).unwrap_or_else(|| "—".into())),
                                    (tr().mount_solve_pixscale,
                                     s.pixscale_arcsec.map(|v| format!("{v:.2}\"/px")).unwrap_or_else(|| "—".into())),
                                ];
                                view! {
                                    <div class="grid grid-cols-[auto_1fr] gap-y-sp-1 gap-x-3 items-baseline">
                                        {rows.into_iter().map(|(lbl, val)| view! {
                                            <span class="font-mono text-sm text-text-faint whitespace-nowrap">{lbl}{":"}</span>
                                            <span class="font-mono text-[12px] text-text-dim tabular-nums">{val}</span>
                                        }).collect::<Vec<_>>()}
                                    </div>
                                }.into_any()
                            }}

                            // Status timeline
                            <div>
                                <div class="font-ui font-semibold text-sm text-text-blue tracking-[0.08em] mb-sp-2">
                                    {move || tr().mount_solve_timeline}
                                </div>
                                <div class="flex flex-col gap-[2px] max-h-[180px] overflow-y-auto">
                                    {move || {
                                        let mut hist = solve.get().history;
                                        if hist.is_empty() {
                                            return view! {
                                                <div class="text-[#555] font-mono text-[12px] py-sp-1">
                                                    {tr().mount_solve_no_solution}
                                                </div>
                                            }.into_any();
                                        }
                                        // newest first
                                        hist.reverse();
                                        view! {
                                            <For
                                                each=move || hist.clone().into_iter().enumerate()
                                                key=|(i, e)| (*i, e.t_ms as u64, e.status.clone())
                                                children=move |(_, e)| {
                                                    let cls = format!(
                                                        "font-mono text-[12px] {}",
                                                        align_status_class(&e.status),
                                                    );
                                                    view! {
                                                        <div class="grid grid-cols-[auto_1fr] gap-x-3 items-baseline">
                                                            <span class="font-mono text-[11px] text-text-faint tabular-nums">
                                                                {fmt_clock(e.t_ms)}
                                                            </span>
                                                            <span class=cls>{e.status}</span>
                                                        </div>
                                                    }
                                                }
                                            />
                                        }.into_any()
                                    }}
                                </div>
                            </div>

                            // Full solver log
                            <div class="flex-1 min-h-0 flex flex-col">
                                <div class="font-ui font-semibold text-sm text-text-blue tracking-[0.08em] mb-sp-2">
                                    {move || tr().mount_solve_log}
                                </div>
                                <pre class="flex-1 min-h-[120px] max-h-[320px] overflow-auto m-0 p-sp-3 bg-[rgba(0,0,0,0.35)] border border-border-base rounded-sm font-mono text-[11px] leading-[1.5] text-text-dim whitespace-pre-wrap break-words">
                                    {move || {
                                        let log = solve.get().log;
                                        if log.trim().is_empty() { tr().mount_solve_no_log.to_string() }
                                        else { log }
                                    }}
                                </pre>
                            </div>
                        </div>
                    </div>
                </div>
            </Show>
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

// Tailwind text-colour class for an AlignState string (kstars ekos.h:145).
fn align_status_class(status: &str) -> &'static str {
    let s = status.to_ascii_uppercase();
    if s.contains("SUCCESS") || s.contains("COMPLETE") {
        "text-state-success"
    } else if s.contains("FAIL") || s.contains("ABORT") {
        "text-state-danger"
    } else if s.contains("PROGRESS") || s.contains("SYNC") || s.contains("SLEW")
        || s.contains("ROTAT") || s.contains("SUSPEND")
    {
        "text-state-info"
    } else {
        "text-text-dim"
    }
}

// True while a solve is actively running (used to pulse the Process button).
fn align_in_progress(status: &str) -> bool {
    let s = status.to_ascii_uppercase();
    s.contains("PROGRESS") || s.contains("SYNC") || s.contains("SLEW")
        || s.contains("ROTAT")
}

// HH:MM:SS from a js Date epoch-ms, local time.
fn fmt_clock(t_ms: f64) -> String {
    let d = web_sys::js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(t_ms));
    format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds())
}
