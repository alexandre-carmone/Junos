//! Imaging / Capture module UI — full-screen tab.
//!
//! Talks to KStars via Ekos Live:
//!   - Inbound: `new_capture_state`, `new_camera_state`, `capture_get_all_settings`,
//!     `capture_get_sequences`, `new_preview_image` (non-focus frames).
//!   - Outbound: `capture_start`, `capture_stop`, `capture_loop`, `capture_preview`,
//!     `capture_set_all_settings{…}`, `capture_add_sequence`,
//!     `capture_remove_sequence{index}`, `capture_clear_sequences`,
//!     `device_property_set` on `CCD_TEMPERATURE` / `CCD_COOLER`.
//!     See `kstars/kstars/ekos/ekoslive/message.cpp:453-545` and
//!     `camera_jobs.cpp:865` for the sequence-job JSON shape.

mod fields;
mod jobs;
mod styles;
mod types;
mod util;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::compat::{CameraSnapshot, CaptureSnapshot, FilterWheelSnapshot};
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::{
    dispatch_setting as ws_dispatch_setting, send_cmd, send_device_property_set,
};
use crate::{ActiveTabCtx, RevealInFilesCtx, Tab};
use super::sequence_editor::{build_esq_xml, SeqFrame, SequenceEditor};

use fields::{
    render_exposure_field, render_filter_field, render_frame_type_segmented, render_stacked_field,
};
use jobs::{job_detail_rows, job_status_color, marker_cls};
use styles::{status_color, status_is_active, ACTION_BTN, GHOST_BTN, PANEL_BODY, PANEL_CLS, SUMMARY_CLS};
use types::{SequenceRow, ONE_SHOT_GAIN_FIELDS};
use util::{capture_reveal_path, default_capture_setting_value, event_target_value, initial_preview_visible};

#[component]
pub fn ImagingTab(
    #[prop(into)] capture: Signal<CaptureSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let target_temp = RwSignal::new(-10.0_f64);
    let preview_visible = RwSignal::new(initial_preview_visible());
    let oneshot_open = RwSignal::new(true);
    let editor_open = RwSignal::new(false);
    let job_detail_idx = RwSignal::new(None::<usize>);

    // Imaging's draft sequence — owned locally exactly like Scheduler/Mosaic.
    // Submitted to KStars in one shot via `capture_load_sequence_file {filedata}`.
    let seq_frames: RwSignal<Vec<SeqFrame>> = RwSignal::new(vec![SeqFrame::default()]);

    let on_toggle_preview = move |_: web_sys::MouseEvent| {
        preview_visible.update(|v| *v = !*v);
        if let Some(ls) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = ls.set_item(
                util::PREVIEW_VISIBLE_KEY,
                if preview_visible.get_untracked() { "true" } else { "false" },
            );
        }
    };

    let reveal_ctx = use_context::<RevealInFilesCtx>();
    let active_tab_ctx = use_context::<ActiveTabCtx>();
    let on_reveal_files = move |_| {
        let path = capture.with(|c| capture_reveal_path(&c.settings));
        if let Some(ctx) = reveal_ctx {
            ctx.0.set(path);
        }
        if let Some(ctx) = active_tab_ctx {
            ctx.0.set(Tab::Files);
        }
    };

    // ── Action dispatchers ────────────────────────────────────────────────
    let s_start = send.clone();
    let on_start = move |_| send_cmd(&s_start, "capture_start", serde_json::json!({}));
    let s_stop = send.clone();
    let on_stop = move |_| send_cmd(&s_stop, "capture_stop", serde_json::json!({}));
    let s_preview = send.clone();
    let on_preview = move |_| send_cmd(&s_preview, "capture_preview", serde_json::json!({}));
    let s_loop = send.clone();
    let on_loop = move |_| send_cmd(&s_loop, "capture_loop", serde_json::json!({}));

    let sv_send_seq = StoredValue::new(send.clone());
    let on_send_seq = move |_| {
        let frames = seq_frames.get_untracked();
        if frames.is_empty() {
            return;
        }
        let xml = build_esq_xml("", &frames);
        let s = sv_send_seq.get_value();
        send_cmd(
            &s,
            "capture_load_sequence_file",
            serde_json::json!({ "filedata": xml }),
        );
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(500).await;
            send_cmd(&s, "capture_get_sequences", serde_json::json!({}));
        });
    };
    let s_clear = send.clone();
    let on_clear_seq =
        move |_| send_cmd(&s_clear, "capture_clear_sequences", serde_json::json!({}));

    // ── Save / Load sequence file ─────────────────────────────────────────
    // StoredValue is Copy, so these can be captured by on:click closures inside
    // a <Show> children-closure without turning it FnOnce.
    let save_open = RwSignal::new(false);
    let save_path = RwSignal::new(String::new());
    let sv_save = StoredValue::new(send.clone());

    let load_open = RwSignal::new(false);
    let load_path = RwSignal::new(String::new());
    let sv_load = StoredValue::new(send.clone());

    // ── Cooling → INDI device_property_set on the active camera ───────────
    let s_cool_on = send.clone();
    let cam_cool_on = camera;
    let on_cooler_on = move |_| {
        let dev = cam_cool_on.with(|c| c.device.clone());
        if dev.is_empty() {
            return;
        }
        send_device_property_set(
            &s_cool_on,
            &dev,
            "CCD_COOLER",
            serde_json::json!([
                { "name": "COOLER_ON",  "state": 1 },
                { "name": "COOLER_OFF", "state": 0 },
            ]),
        );
    };
    let s_cool_off = send.clone();
    let cam_cool_off = camera;
    let on_cooler_off = move |_| {
        let dev = cam_cool_off.with(|c| c.device.clone());
        if dev.is_empty() {
            return;
        }
        send_device_property_set(
            &s_cool_off,
            &dev,
            "CCD_COOLER",
            serde_json::json!([
                { "name": "COOLER_ON",  "state": 0 },
                { "name": "COOLER_OFF", "state": 1 },
            ]),
        );
    };
    let s_set_temp = send.clone();
    let cam_set_temp = camera;
    let on_set_temp = move |_| {
        let dev = cam_set_temp.with(|c| c.device.clone());
        if dev.is_empty() {
            return;
        }
        send_device_property_set(
            &s_set_temp,
            &dev,
            "CCD_TEMPERATURE",
            serde_json::json!([
                { "name": "CCD_TEMPERATURE_VALUE", "value": target_temp.get() },
            ]),
        );
    };

    // ── Settings dispatch ────────────────────────────────────────────────
    // Optimistic overrides: user edits pin the value locally until either
    // KStars echoes the same value back (resync) or the user edits again.
    // This guards against KStars overwriting captureGainN with the
    // GainSpinSpecialValue sentinel, and combos (FilterPosCombo, captureISOS,
    // captureTypeS) being silently reset when KStars's widget can't apply
    // the value (items not yet populated, asynchronous re-stamp from
    // FilterManager::positionChanged, etc.).
    let overrides: RwSignal<std::collections::HashMap<&'static str, serde_json::Value>> =
        RwSignal::new(std::collections::HashMap::new());
    let s_set_all = send.clone();
    let dispatch_setting = move |key: &'static str, value: serde_json::Value| {
        overrides.update(|m| {
            m.insert(key, value.clone());
        });
        ws_dispatch_setting(&s_set_all, "capture_set_all_settings", None, key, value);
    };

    // ── Sequence queue rendering ──────────────────────────────────────────
    // Sequence job JSON keys come from kstars camera_jobs.cpp::createJsonJob:
    // {Status, Filter, Count, Exp, Type, Bin, "ISO/Gain", Offset, Encoding,
    //  Format, Temperature, ...}. All capitalised. Count/Exp are strings.
    let sequence_rows = move || {
        let cap = capture.get();
        let seq = &cap.sequence;
        let Some(arr) = seq.as_array() else {
            return Vec::new();
        };
        // Live frame count from new_capture_state (seqv / seqr)
        let live_current = cap.seq_current;
        let live_total = cap.seq_total;
        arr.iter()
            .enumerate()
            .map(|(i, job)| {
                let count_raw = job["Count"].as_str().unwrap_or("0/0");
                let (completed, total) = match count_raw.split_once('/') {
                    Some((c, t)) => (c.trim().to_string(), t.trim().to_string()),
                    None => (String::new(), count_raw.to_string()),
                };
                let status = job["Status"].as_str().unwrap_or("Idle").to_string();
                // For the active job, override with live seqv/seqr counts
                let (completed, total) = if status == "In Progress" {
                    (
                        live_current.map(|v| v.to_string()).unwrap_or(completed),
                        live_total.map(|v| v.to_string()).unwrap_or(total),
                    )
                } else {
                    (completed, total)
                };
                let exp = job["Exp"].as_str().unwrap_or("—").to_string();
                let ftype = job["Type"].as_str().unwrap_or("").to_string();
                let filter = job["Filter"].as_str().unwrap_or("").to_string();
                let bin = job["Bin"].as_str().unwrap_or("").to_string();
                SequenceRow {
                    index: i,
                    completed,
                    total,
                    exp,
                    ftype,
                    filter,
                    bin,
                    status,
                }
            })
            .collect::<Vec<_>>()
    };

    let sv_remove_job = StoredValue::new(send.clone());
    let on_remove_job = move |idx: usize| {
        sv_remove_job.with_value(|s| {
            send_cmd(
                s,
                "capture_remove_sequence",
                serde_json::json!({ "index": idx }),
            )
        });
    };

    // Shared setting lookup: returns the current Value from the debounced
    // capture_get_all_settings snapshot, or Null.
    // For captureGainN/captureOffsetN, KStars surfaces a "no value" sentinel
    // equal to `min - step` (e.g. -10 when min=0 step=10). Treat any negative
    // numeric value for those keys as absent and fall back to our default.
    // Local overrides (set by the user via `dispatch_setting`) take priority.
    let get_setting = move |key: &'static str| -> serde_json::Value {
        if let Some(v) = overrides.with(|m| m.get(key).cloned()) {
            return v;
        }
        capture.with(|c| {
            c.settings
                .as_object()
                .and_then(|o| o.get(key).cloned())
                .filter(|v| {
                    !(matches!(key, "captureGainN" | "captureOffsetN")
                        && v.as_f64().map(|n| n < 0.0).unwrap_or(false))
                })
                .or_else(|| default_capture_setting_value(key))
                .unwrap_or(serde_json::Value::Null)
        })
    };

    // Resync: when the server snapshot matches an override, drop the override
    // so future out-of-band changes flow through normally.
    Effect::new(move |_| {
        let snapshot = capture.with(|c| c.settings.clone());
        let Some(obj) = snapshot.as_object() else {
            return;
        };
        let to_remove: Vec<&'static str> = overrides.with(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    obj.get(*k).filter(|server| *server == v).map(|_| *k)
                })
                .collect()
        });
        if !to_remove.is_empty() {
            overrides.update(|m| {
                for k in to_remove {
                    m.remove(k);
                }
            });
        }
    });

    // Prime captureGainN once: when KStars first reports settings without a
    // real gain (missing or negative sentinel), push our default (100) so the
    // value sticks server-side for sequence jobs.
    let prime_dispatch = StoredValue::new(send.clone());
    let primed = StoredValue::new(false);
    Effect::new(move |_| {
        let has_settings =
            capture.with(|c| c.settings.as_object().map(|o| !o.is_empty()).unwrap_or(false));
        if !has_settings || primed.get_value() {
            return;
        }
        let needs_default = capture.with(|c| {
            c.settings
                .as_object()
                .and_then(|o| o.get("captureGainN"))
                .map(|v| v.as_f64().map(|n| n < 0.0).unwrap_or(true))
                .unwrap_or(true)
        });
        if needs_default {
            if let Some(default) = default_capture_setting_value("captureGainN") {
                prime_dispatch.with_value(|s| {
                    ws_dispatch_setting(s, "capture_set_all_settings", None, "captureGainN", default);
                });
            }
        }
        primed.set_value(true);
    });

    let stat_label = "text-text-blue text-xs uppercase tracking-[0.06em]";

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono overflow-y-auto overflow-x-hidden [-webkit-tap-highlight-color:rgba(136,170,255,0.25)]">

            // ── Header ────────────────────────────────────────────────────
            <div class="flex flex-wrap items-center gap-y-[10px] gap-x-[18px] py-[10px] pl-20 pr-5 border-b border-border-base bg-[rgba(6,6,15,0.92)] text-md min-h-[44px] max-[759px]:py-[6px] max-[759px]:pl-16 max-[759px]:pr-2 max-[759px]:gap-y-[4px] max-[759px]:gap-x-2 max-[759px]:text-xs max-[374px]:gap-x-[6px] max-[374px]:gap-y-[3px]">
                <span
                    class="inline-block py-sp-1 px-sp-3 rounded-[14px] text-sm border border-current"
                    class:animate-pulse=move || status_is_active(&capture.with(|c| c.status.clone()))
                    style=move || format!(
                        "color:{};",
                        status_color(&capture.with(|c| c.status.clone()))
                    )>
                    {move || {
                        let s = capture.with(|c| c.status.clone());
                        if s.is_empty() { tr().idle.to_string() } else { s }
                    }}
                </span>
                <span class="inline-flex items-center gap-[6px] max-[479px]:hidden">
                    <span class=stat_label>{move || tr().imaging_camera}</span>
                    <span>{move || {
                        let d = camera.with(|c| c.device.clone());
                        if d.is_empty() { "—".to_string() } else { d }
                    }}</span>
                </span>
                <span class="inline-flex items-center gap-[6px]">
                    <span class=stat_label>{move || tr().imaging_temp}</span>
                    <span>{move || camera.with(|c| c.temperature
                        .map(|v| format!("{:.1}°C", v))
                        .unwrap_or_else(|| "—".into()))}</span>
                </span>
                <span class="inline-flex items-center gap-[6px] max-[759px]:hidden">
                    <span class=stat_label>{move || tr().imaging_cooler}</span>
                    <span
                        style=move || {
                            let on = camera.with(|c| c.cooler_on).unwrap_or(false);
                            format!("color:{};", if on { "var(--state-ok)" } else { "var(--text-muted)" })
                        }>
                        {move || match camera.with(|c| c.cooler_on) {
                            Some(true)  => tr().imaging_cooler_on_val.to_string(),
                            Some(false) => tr().imaging_cooler_off_val.to_string(),
                            None        => "—".to_string(),
                        }}
                    </span>
                </span>
                <span class="inline-flex items-center gap-[6px] max-[759px]:hidden">
                    <span class=stat_label>{move || tr().imaging_sensor}</span>
                    <span>{move || camera.with(|c| match (c.sensor_width, c.sensor_height) {
                        (Some(w), Some(h)) => format!("{}×{}", w, h),
                        _ => "—".into(),
                    })}</span>
                </span>
                <span class="inline-flex items-center gap-[6px] max-[639px]:hidden">
                    <span class=stat_label>{move || tr().imaging_progress}</span>
                    <span>{move || capture.with(|c| match (c.seq_current, c.seq_total) {
                        (Some(a), Some(b)) => format!("{} / {}", a, b),
                        _ => "—".into(),
                    })}</span>
                </span>
                <div class="inline-flex flex-wrap items-center gap-[6px] py-[2px] px-[8px] border border-[#23283b] bg-[rgba(10,12,20,0.55)] rounded-sm max-[759px]:order-last max-[759px]:w-full max-[759px]:justify-between max-[759px]:px-[6px]">
                    <span class="text-text-blue text-[10px] uppercase tracking-[0.06em] max-[759px]:hidden">{move || tr().imaging_cooling}</span>
                    <span class="text-text-blue text-xs">{move || tr().imaging_target_c}</span>
                    <input
                        type="number"
                        step="0.5"
                        value=move || format!("{:.1}", target_temp.get())
                        on:change=move |ev| {
                            let s = event_target_value(&ev);
                            if let Ok(n) = s.parse::<f64>() { target_temp.set(n); }
                        }
                        class="input input--sm w-[72px] font-mono"
                    />
                    <button on:click=on_set_temp class=GHOST_BTN>{move || tr().imaging_set}</button>
                    <button on:click=on_cooler_on class="btn btn--sm btn-ghost text-text-blue max-[479px]:hidden">{move || tr().cooler_on}</button>
                    <button on:click=on_cooler_off class="btn btn--sm btn-ghost text-text-blue max-[479px]:hidden">{move || tr().cooler_off}</button>
                </div>
                <button
                    class=format!("{GHOST_BTN} ml-auto max-[639px]:ml-0")
                    on:click=on_toggle_preview
                    title=move || tr().imaging_toggle_preview_title>
                    {move || if preview_visible.get() { tr().imaging_hide_preview } else { tr().imaging_show_preview }}
                </button>
                <button
                    class="btn btn--sm btn-ghost text-text-blue max-[639px]:hidden"
                    on:click=on_reveal_files
                    title=move || tr().files_open_in_files>
                    {move || tr().files_open_in_files}
                </button>
            </div>

            // ── Activity strip: live exposure + time-remaining readout ─────
            // Only shown once KStars has reported a real capture status (i.e.
            // not the blank/idle default). All fields come straight from the
            // already-populated `CaptureSnapshot`; no extra plumbing needed.
            {move || {
                let status = capture.with(|c| c.status.clone());
                let active = status_is_active(&status);
                let bar_color = status_color(&status);
                let show = !status.is_empty() && !status.eq_ignore_ascii_case("idle");
                if !show {
                    return ().into_any();
                }

                // Current-frame countdown. KStars counts down, so the filled
                // portion of the bar is `1 - left/total`.
                let (exp_label, exp_fill) = capture.with(|c| {
                    match (c.exposure_left, c.exposure_total) {
                        (Some(left), Some(total)) if total > 0.0 => {
                            let fill = ((1.0 - left / total) * 100.0).clamp(0.0, 100.0);
                            (format!("{:.1}s / {:.0}s", left.max(0.0), total), fill)
                        }
                        (Some(left), _) => (format!("{:.1}s", left.max(0.0)), 0.0),
                        _ => ("—".to_string(), 0.0),
                    }
                });

                let overall_pct = capture.with(|c| c.progress).unwrap_or(0.0).clamp(0.0, 100.0);
                let seq_time = capture.with(|c| c.seq_remaining_time.clone());
                let overall_time = capture.with(|c| c.overall_remaining_time.clone());
                let dash = "--:--:--".to_string();
                let seq_time = if seq_time.is_empty() { dash.clone() } else { seq_time };
                let overall_time = if overall_time.is_empty() { dash } else { overall_time };
                let last_log = capture.with(|c| c.log
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string());

                let stat_label_lc = "text-text-blue text-[10px] uppercase tracking-[0.06em] shrink-0";
                let track_cls = "relative flex-1 min-w-[60px] h-[6px] rounded-full bg-[rgba(255,255,255,0.08)] overflow-hidden";

                view! {
                    <div class="flex flex-col gap-[7px] py-[8px] pl-20 pr-5 border-b border-border-base bg-[rgba(8,10,18,0.6)] max-[759px]:pl-16 max-[759px]:pr-3 max-[759px]:py-[6px]">
                        // Row 1 — current frame exposure countdown
                        <div class="flex items-center gap-[10px] text-xs">
                            <span class=stat_label_lc>{move || tr().imaging_frame_exposure}</span>
                            <span class="shrink-0 font-mono tabular-nums" style=format!("color:{};", bar_color)>{exp_label}</span>
                            <div class=track_cls>
                                <div
                                    class=move || format!("absolute inset-y-0 left-0 rounded-full transition-[width] duration-300 {}", if active { "animate-pulse" } else { "" })
                                    style=format!("width:{:.1}%;background:{};", exp_fill, bar_color)
                                ></div>
                            </div>
                            <span class="shrink-0 font-mono tabular-nums text-text-blue w-[44px] text-right">{format!("{:.0}%", overall_pct)}</span>
                        </div>
                        // Row 2 — sequence + overall time remaining
                        <div class="flex items-center gap-[10px] text-xs">
                            <span class=stat_label_lc>{move || tr().imaging_seq_remaining}</span>
                            <span class="shrink-0 font-mono tabular-nums text-text">{seq_time}</span>
                            <span class="text-text-muted px-[2px]">"•"</span>
                            <span class=stat_label_lc>{move || tr().imaging_overall_remaining}</span>
                            <span class="shrink-0 font-mono tabular-nums text-text">{overall_time}</span>
                            <div class=track_cls>
                                <div
                                    class="absolute inset-y-0 left-0 rounded-full transition-[width] duration-300"
                                    style=format!("width:{:.1}%;background:{};", overall_pct, bar_color)
                                ></div>
                            </div>
                        </div>
                        // Last log line
                        {(!last_log.is_empty()).then(|| {
                            let log_title = last_log.clone();
                            view! {
                                <div class="flex items-center gap-[8px] text-[11px] min-w-0">
                                    <span class=stat_label_lc>{move || tr().imaging_log_label}</span>
                                    <span class="text-text-muted truncate min-w-0" title=log_title>{last_log}</span>
                                </div>
                            }
                        })}
                    </div>
                }.into_any()
            }}

            // ── Body: sequence-centred vertical layout ─────────────────────
            <div class="overflow-x-hidden p-sp-4 pb-6 flex flex-col gap-sp-4 max-[759px]:p-sp-3">
                <div
                    class=move || {
                        let base = "min-h-[180px] max-h-[35vh] overflow-hidden flex items-center justify-center bg-bg-input-deep border border-border-base rounded-[3px]";
                        if preview_visible.get() { base.to_string() } else { format!("{base} hidden") }
                    }>
                    {move || match capture.with(|c| c.preview_url.clone()) {
                        Some(url) => view! {
                            <img class="max-w-full max-h-[35vh] object-contain [image-rendering:pixelated]" src=url />
                        }.into_any(),
                        None => view! {
                            <div class="text-[#444] text-sm text-center px-3">
                                {move || tr().imaging_no_frame}
                            </div>
                        }.into_any(),
                    }}
                </div>

                <details
                    class=PANEL_CLS
                    prop:open=move || oneshot_open.get()
                    on:toggle=move |ev: web_sys::Event| {
                        if let Some(el) = ev.target()
                            .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                        { oneshot_open.set(el.open()); }
                    }>
                    <summary class=SUMMARY_CLS>
                        <span class=move || marker_cls(oneshot_open.get())>"▸"</span>
                        {move || tr().imaging_one_shot}
                    </summary>
                    <div class=PANEL_BODY>
                        <div class="flex flex-col gap-sp-3 mb-sp-3">
                            // Exposure: bespoke widget — large numeric input
                            // accepting 0.001s–3600s, with quick-pick chips below.
                            {render_exposure_field(lang, get_setting, dispatch_setting.clone())}

                            // Frame type: segmented control instead of <select>.
                            {render_frame_type_segmented(lang, camera, get_setting, dispatch_setting.clone())}

                            // Filter / Gain / ISO row — three equal columns
                            // with the label stacked above the editor. Filter
                            // gets its own renderer because changing the combo
                            // via capture_set_all_settings does not move the
                            // wheel in KStars (camera.cpp:245); we have to
                            // hit the FilterWheel device's FILTER_SLOT INDI
                            // property directly.
                            <div class="grid grid-cols-3 gap-sp-3 max-[479px]:grid-cols-1">
                                {render_filter_field(lang, filter_wheel, get_setting, dispatch_setting.clone(), send.clone())}
                                {ONE_SHOT_GAIN_FIELDS.iter().map(|f| {
                                    render_stacked_field(*f, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())
                                }).collect::<Vec<_>>()}
                            </div>
                        </div>
                        <div class="grid grid-cols-4 gap-sp-2 max-[759px]:grid-cols-2">
                            <button on:click=on_preview class=ACTION_BTN style="--btn-color:var(--state-info);">{move || tr().preview}</button>
                            <button on:click=on_loop class=ACTION_BTN style="--btn-color:var(--state-info);">{move || tr().focus_loop_btn}</button>
                            <button on:click=on_start class=ACTION_BTN style="--btn-color:var(--state-ok);">{move || tr().start}</button>
                            <button on:click=on_stop class=ACTION_BTN style="--btn-color:var(--state-err);">{move || tr().stop}</button>
                        </div>
                    </div>
                </details>

                // ─ Sequence queue ────────────────────────────────────────
                <section class="flex flex-col min-w-0 border border-border-base bg-[rgba(8,10,18,0.45)] rounded-[3px] overflow-visible">
                    <div class="flex flex-wrap items-center justify-between gap-sp-2 pt-3 pb-sp-2 px-sp-4 border-b border-border-base max-[899px]:flex-col max-[899px]:items-stretch">
                        <span class="text-text-blue text-sm uppercase tracking-[0.08em]">{move || tr().imaging_sequence_queue}</span>
                        <div class="flex flex-wrap gap-[6px] max-[479px]:grid max-[479px]:grid-cols-2 max-[479px]:gap-sp-2 max-[479px]:w-full">
                            <button on:click=move |_| editor_open.set(true) class=ACTION_BTN style="--btn-color:var(--state-info);">{move || tr().imaging_sequence_editor}</button>
                            <button on:click=on_clear_seq class=ACTION_BTN style="--btn-color:var(--state-err);">{move || tr().seq_clear}</button>
                            <button
                                class=ACTION_BTN
                                style="--btn-color:var(--state-ok);"
                                on:click=move |_| { save_open.update(|v| *v = !*v); load_open.set(false); }>
                                {move || tr().save_profile}
                            </button>
                            <button
                                class=ACTION_BTN
                                style="--btn-color:var(--state-warn);"
                                on:click=move |_| { load_open.update(|v| *v = !*v); save_open.set(false); }>
                                {move || tr().load_profile}
                            </button>
                        </div>
                    </div>
                    // Save inline row
                    <Show when=move || save_open.get()>
                        <div class="flex gap-[6px] py-[6px] px-sp-2 bg-[#0d1a12] border-b border-[#224433]">
                            <input
                                type="text"
                                placeholder="/home/user/seq.esq"
                                prop:value=move || save_path.get()
                                on:input=move |ev| save_path.set(event_target_value(&ev))
                                class="flex-1 bg-bg-input text-[#c0ffd0] border border-[#335544] py-1 px-sp-2 font-mono text-sm"
                            />
                            <button class=ACTION_BTN style="--btn-color:var(--state-ok);" on:click=move |_| {
                                let path = save_path.get_untracked();
                                if !path.is_empty() {
                                    sv_save.with_value(|s| send_cmd(s, "capture_save_sequence_file", serde_json::json!({"filepath": path})));
                                    save_open.set(false);
                                }
                            }>"✓"</button>
                            <button class=ACTION_BTN style="--btn-color:var(--text-faint);" on:click=move |_| save_open.set(false)>"✕"</button>
                        </div>
                    </Show>
                    // Load inline row
                    <Show when=move || load_open.get()>
                        <div class="flex gap-[6px] py-[6px] px-sp-2 bg-[#1a1200] border-b border-[#443322]">
                            <input
                                type="text"
                                placeholder="/home/user/seq.esq"
                                prop:value=move || load_path.get()
                                on:input=move |ev| load_path.set(event_target_value(&ev))
                                class="flex-1 bg-bg-input text-[#ffd0aa] border border-[#554433] py-1 px-sp-2 font-mono text-sm"
                            />
                            <button class=ACTION_BTN style="--btn-color:var(--state-warn);" on:click=move |_| {
                                let path = load_path.get_untracked();
                                if !path.is_empty() {
                                    sv_load.with_value(|s| send_cmd(s, "capture_load_sequence_file", serde_json::json!({"filepath": path})));
                                    load_open.set(false);
                                    let s2 = sv_load.get_value();
                                    wasm_bindgen_futures::spawn_local(async move {
                                        gloo_timers::future::TimeoutFuture::new(500).await;
                                        send_cmd(&s2, "capture_get_sequences", serde_json::json!({}));
                                    });
                                }
                            }>"✓"</button>
                            <button class=ACTION_BTN style="--btn-color:var(--text-faint);" on:click=move |_| load_open.set(false)>"✕"</button>
                        </div>
                    </Show>
                    <div class="py-sp-2 px-sp-3">
                        {move || {
                            let rows = sequence_rows();
                            if rows.is_empty() {
                                return view! {
                                    <div class="text-[#555] text-sm py-3 px-[6px]">
                                        {tr().imaging_empty_queue}
                                    </div>
                                }.into_any();
                            }
                            rows.into_iter().map(|r| {
                                let on_remove = on_remove_job.clone();
                                let idx = r.index;
                                let badge_color = job_status_color(&r.status);
                                let filter_label = if r.filter.is_empty() { "—".into() } else { r.filter };
                                view! {
                                    <div
                                        class="w-full text-left flex flex-col gap-[5px] py-sp-2 px-sp-3 mb-[6px] bg-[rgba(14,16,26,0.85)] border border-[#22263a] rounded-sm hover:border-[#3a4465] hover:bg-[rgba(18,22,36,0.92)] transition-colors cursor-pointer"
                                        on:click=move |_| job_detail_idx.set(Some(idx))>
                                        <div class="flex items-center gap-sp-2">
                                            <span class="text-[#555] text-xs">{format!("#{}", idx + 1)}</span>
                                            <span
                                                class="text-[9px] font-bold uppercase tracking-[0.06em] py-[1px] px-[7px] rounded-[3px] text-[#0a0c14] whitespace-nowrap"
                                                style:background=badge_color>
                                                {r.status}
                                            </span>
                                            <button
                                                class="btn btn--sm btn-ghost text-state-err"
                                                title=tr().imaging_remove_job
                                                on:click=move |ev: web_sys::MouseEvent| {
                                                    ev.stop_propagation();
                                                    on_remove(idx);
                                                }>
                                                "×"
                                            </button>
                                        </div>
                                        <div class="flex flex-wrap gap-sp-3 text-[#7a88a8] text-xs">
                                            <span class="text-[#aab8d0] text-sm whitespace-nowrap">{r.ftype}</span>
                                            <span class="text-[#333] text-xs">"|"</span>
                                            <span class="text-[#aab8d0] text-sm whitespace-nowrap">{format!("{} s", r.exp)}</span>
                                            <span class="text-[#333] text-xs">"|"</span>
                                            <span class="text-[#aab8d0] text-sm whitespace-nowrap">{filter_label}</span>
                                            <span class="text-[#333] text-xs">"|"</span>
                                            <span class="text-text-dim text-sm font-bold whitespace-nowrap">
                                                {format!("{} / {}", r.completed, r.total)}
                                            </span>
                                        </div>
                                    </div>
                                }.into_any()
                            }).collect::<Vec<_>>().into_any()
                        }}
                    </div>
                </section>

                // ─ Sequence editor (full-screen overlay) ─────────────────
                <Show when=move || editor_open.get()>
                    <div class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2">
                        <div class="w-full max-w-[980px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col">
                            <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                                <h2 class="text-text-blue text-sm uppercase tracking-[0.08em]">{move || tr().imaging_sequence_editor}</h2>
                                <button class=GHOST_BTN on:click=move |_| editor_open.set(false)>{move || tr().imaging_close}</button>
                            </div>
                            <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 max-[759px]:p-sp-3">
                                <SequenceEditor frames=seq_frames camera=camera filter_wheel=filter_wheel />
                            </div>
                            <div class="flex justify-end gap-sp-2 py-sp-3 px-sp-4 border-t border-border-base bg-[rgba(10,12,20,0.8)]">
                                <button class=GHOST_BTN on:click=move |_| editor_open.set(false)>{move || tr().imaging_close}</button>
                                <button class=ACTION_BTN style="--btn-color:var(--state-info);" on:click=move |ev| {
                                    on_send_seq(ev);
                                    editor_open.set(false);
                                }>{move || tr().imaging_send_sequence}</button>
                            </div>
                        </div>
                    </div>
                </Show>

                <Show when=move || job_detail_idx.get().is_some()>
                    {move || {
                        let idx = job_detail_idx.get().unwrap_or(0);
                        let job = capture.with(|c| c.sequence.as_array().and_then(|arr| arr.get(idx).cloned()));
                        let Some(job) = job else {
                            return view! {}.into_any();
                        };
                        let rows = job_detail_rows(&job, tr());
                        let on_remove = on_remove_job.clone();
                        view! {
                            <div class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.82)] backdrop-blur-sm flex items-center justify-center p-sp-4 max-[759px]:items-stretch max-[759px]:p-sp-2">
                                <div class="w-full max-w-[720px] max-h-[90vh] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col">
                                    <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                                        <h2 class="text-text-blue text-sm uppercase tracking-[0.08em]">{format!("{} #{}", tr().imaging_job_detail, idx + 1)}</h2>
                                        <button class=GHOST_BTN on:click=move |_| job_detail_idx.set(None)>{move || tr().imaging_close}</button>
                                    </div>
                                    <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 grid grid-cols-[repeat(auto-fit,minmax(220px,1fr))] gap-sp-2 max-[759px]:p-sp-3">
                                        {rows.into_iter().map(|(label, value)| view! {
                                            <div class="border border-[#22263a] bg-[rgba(14,16,26,0.8)] rounded-sm py-sp-2 px-sp-3">
                                                <div class="text-text-blue text-xs uppercase tracking-[0.06em] mb-[3px]">{label}</div>
                                                <div class="text-sm text-[#cbd6f0] break-words">{value}</div>
                                            </div>
                                        }).collect::<Vec<_>>()}
                                    </div>
                                    <div class="flex justify-end gap-sp-2 py-sp-3 px-sp-4 border-t border-border-base bg-[rgba(10,12,20,0.8)]">
                                        <button class=GHOST_BTN on:click=move |_| job_detail_idx.set(None)>{move || tr().imaging_close}</button>
                                        <button class=ACTION_BTN style="--btn-color:var(--state-err);" on:click=move |_| {
                                            on_remove.clone()(idx);
                                            job_detail_idx.set(None);
                                        }>{move || tr().imaging_remove_job}</button>
                                    </div>
                                </div>
                            </div>
                        }.into_any()
                    }}
                </Show>
            </div>
        </div>
    }
}
