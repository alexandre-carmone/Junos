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

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::compat::{CameraSnapshot, CaptureSnapshot, FilterWheelSnapshot};
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;
use crate::ws_helpers::{
    dispatch_setting as ws_dispatch_setting, send_cmd, send_device_property_set,
};
use crate::{ActiveTabCtx, RevealInFilesCtx, Tab};

// ── Shared Tailwind class fragments ───────────────────────────────────────────
// Repeating chrome — buttons, inputs, foldable panels — kept here so each
// `view! {}` doesn't carry the same long string 4× over.
const GHOST_BTN: &str = "btn btn--sm btn-ghost text-text-blue";
const ACTION_BTN: &str = "btn btn--sm !border-[color:var(--btn-color,var(--text-blue))] text-[color:var(--btn-color,var(--text-blue))]";
const FIELD_INPUT: &str = "input input--sm flex-1 min-w-0 font-mono";
const FIELD_LABEL: &str = "basis-[120px] grow-0 shrink-0 text-text-blue overflow-hidden text-ellipsis whitespace-nowrap max-[479px]:basis-auto max-[479px]:text-xs";
const PANEL_CLS: &str =
    "border border-border-base bg-[rgba(10,12,20,0.55)] rounded-[3px] overflow-hidden";
const SUMMARY_CLS: &str = "list-none cursor-pointer py-sp-2 px-3 text-text-blue text-sm font-bold uppercase tracking-[0.08em] flex items-center gap-sp-2 select-none hover:bg-[rgba(20,24,40,0.7)] [&::-webkit-details-marker]:hidden";
const PANEL_BODY: &str = "py-sp-3 px-3 pb-3 border-t border-[#1a1c28]";

fn status_color(status: &str) -> &'static str {
    let s = status.to_lowercase();
    if s.contains("error") || s.contains("abort") || s.contains("fail") {
        "var(--state-err)"
    } else if s.contains("complete") {
        "var(--state-ok)"
    } else if s.contains("capturing") || s.contains("progress") {
        "var(--state-info)"
    } else if s.contains("image received") || s.contains("frame") {
        "var(--state-info)"
    } else if s.contains("waiting") || s.contains("pause") {
        "var(--state-warn)"
    } else {
        "var(--text-muted)"
    }
}

// Field declarations: maps a KStars widget objectName (defined in
// `kstars/ekos/capture/camera.ui`) to a human label and editor kind. The keys
// are what comes back from `capture_get_all_settings` and what
// `capture_set_all_settings` expects.
//
// Light/Dark/Bias/Flat are KStars' canonical frame-type strings (matches
// Scheduler's frame-type select and `SequenceJob` XML).
const FRAME_TYPE_FALLBACK: &[&str] = &["Light", "Dark", "Bias", "Flat"];

#[derive(Clone, Copy)]
enum Kind {
    Number,
    Text,
    Bool,
    /// Dropdown whose options come from the active camera / filter wheel.
    /// The closure receives both snapshots and returns the option list — if
    /// it returns empty, the field renders as a free text input so the user
    /// can still type a value before the device pushes its property.
    ComboDynamic(fn(&CameraSnapshot, &FilterWheelSnapshot) -> Vec<String>),
    /// Filter dropdown — always rendered as `<select>`. When the option
    /// list is empty (no filter wheel attached / not yet reporting) it
    /// shows a single disabled placeholder option from i18n; never falls
    /// back to a free-text input.
    ComboFilter(fn(&CameraSnapshot, &FilterWheelSnapshot) -> Vec<String>),
}

#[derive(Clone, Copy)]
struct Field {
    key: &'static str,
    label: fn(&Translations) -> &'static str,
    kind: Kind,
}

const EXPOSURE_FIELDS: &[Field] = &[
    Field {
        key: "captureExposureN",
        label: |t| t.field_exposure_s,
        kind: Kind::Number,
    },
    Field {
        key: "captureTypeS",
        label: |t| t.field_frame_type,
        kind: Kind::ComboDynamic(|c, _| {
            if c.frame_type_options.is_empty() {
                FRAME_TYPE_FALLBACK.iter().map(|s| s.to_string()).collect()
            } else {
                c.frame_type_options.clone()
            }
        }),
    },
    Field {
        key: "captureCountN",
        label: |t| t.field_count,
        kind: Kind::Number,
    },
    Field {
        key: "captureDelayN",
        label: |t| t.field_delay_s,
        kind: Kind::Number,
    },
];

const ONE_SHOT_EXPOSURE_FIELDS: &[Field] = &[
    Field {
        key: "captureExposureN",
        label: |t| t.field_exposure_s,
        kind: Kind::Number,
    },
    Field {
        key: "captureTypeS",
        label: |t| t.field_frame_type,
        kind: Kind::ComboDynamic(|c, _| {
            if c.frame_type_options.is_empty() {
                FRAME_TYPE_FALLBACK.iter().map(|s| s.to_string()).collect()
            } else {
                c.frame_type_options.clone()
            }
        }),
    },
];

const FRAME_FIELDS: &[Field] = &[
    Field {
        key: "captureBinHN",
        label: |t| t.field_bin_x,
        kind: Kind::Number,
    },
    Field {
        key: "captureBinVN",
        label: |t| t.field_bin_y,
        kind: Kind::Number,
    },
    Field {
        key: "captureFormatS",
        label: |t| t.field_format,
        kind: Kind::ComboDynamic(|c, _| c.capture_format_options.clone()),
    },
    Field {
        key: "captureEncodingS",
        label: |t| t.field_encoding,
        kind: Kind::ComboDynamic(|c, _| c.transfer_format_options.clone()),
    },
];

const GAIN_FIELDS: &[Field] = &[
    Field {
        key: "captureGainN",
        label: |t| t.field_gain,
        kind: Kind::Number,
    },
    Field {
        key: "captureOffsetN",
        label: |t| t.field_offset,
        kind: Kind::Number,
    },
    Field {
        key: "captureISOS",
        label: |t| t.field_iso,
        kind: Kind::ComboDynamic(|c, _| c.iso_options.clone()),
    },
];

const ONE_SHOT_GAIN_FIELDS: &[Field] = &[
    Field {
        key: "captureGainN",
        label: |t| t.field_gain,
        kind: Kind::Number,
    },
    Field {
        key: "captureISOS",
        label: |t| t.field_iso,
        kind: Kind::ComboDynamic(|c, _| c.iso_options.clone()),
    },
];

const FILTER_FIELDS: &[Field] = &[Field {
    key: "FilterPosCombo",
    label: |t| t.field_filter,
    kind: Kind::ComboFilter(|_, fw| fw.filter_names.clone()),
}];

const TARGET_FIELDS: &[Field] = &[
    Field {
        key: "targetNameT",
        label: |t| t.field_target_name,
        kind: Kind::Text,
    },
    Field {
        key: "fileDirectoryT",
        label: |t| t.field_directory,
        kind: Kind::Text,
    },
];

const ENFORCE_TEMP_FIELDS: &[Field] = &[
    Field {
        key: "cameraTemperatureS",
        label: |t| t.field_enforce_temp,
        kind: Kind::Bool,
    },
    Field {
        key: "cameraTemperatureN",
        label: |t| t.field_job_temp_c,
        kind: Kind::Number,
    },
];

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
    let add_overlay_open = RwSignal::new(false);
    let job_detail_idx = RwSignal::new(None::<usize>);

    let on_toggle_preview = move |_: web_sys::MouseEvent| {
        preview_visible.update(|v| *v = !*v);
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

    let sv_add_job = StoredValue::new(send.clone());
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
    let s_set_all = send.clone();
    let dispatch_setting = move |key: &'static str, value: serde_json::Value| {
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
    let get_setting = move |key: &'static str| -> serde_json::Value {
        capture.with(|c| {
            c.settings
                .as_object()
                .and_then(|o| o.get(key).cloned())
                .or_else(|| default_capture_setting_value(key))
                .unwrap_or(serde_json::Value::Null)
        })
    };

    let stat_label = "text-text-blue text-xs uppercase tracking-[0.06em]";

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono overflow-y-auto overflow-x-hidden [-webkit-tap-highlight-color:rgba(136,170,255,0.25)]">

            // ── Header ────────────────────────────────────────────────────
            <div class="flex flex-wrap items-center gap-y-[10px] gap-x-[18px] py-[10px] pl-20 pr-5 border-b border-border-base bg-[rgba(6,6,15,0.92)] text-md min-h-[44px] max-[759px]:py-[6px] max-[759px]:pl-16 max-[759px]:pr-2 max-[759px]:gap-y-[4px] max-[759px]:gap-x-2 max-[759px]:text-xs max-[374px]:gap-x-[6px] max-[374px]:gap-y-[3px]">
                <span
                    class="inline-block py-sp-1 px-sp-3 rounded-[14px] text-sm border border-current"
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
                        <div class="grid grid-cols-[repeat(auto-fit,minmax(260px,1fr))] gap-sp-3 mb-sp-3">
                            {render_group(ONE_SHOT_EXPOSURE_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
                            <div class="flex flex-col gap-[6px]">
                                {render_group(FILTER_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
                                {render_group(ONE_SHOT_GAIN_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
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
                            <button on:click=move |_| add_overlay_open.set(true) class=ACTION_BTN style="--btn-color:var(--state-info);">{move || tr().imaging_add_exposure}</button>
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

                <Show when=move || add_overlay_open.get()>
                    <div class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2">
                        <div class="w-full max-w-[980px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col">
                            <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                                <h2 class="text-text-blue text-sm uppercase tracking-[0.08em]">{move || tr().imaging_add_exposure}</h2>
                                <button class=GHOST_BTN on:click=move |_| add_overlay_open.set(false)>{move || tr().imaging_close}</button>
                            </div>
                            <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 grid grid-cols-[repeat(auto-fit,minmax(280px,1fr))] gap-sp-4 max-[759px]:p-sp-3">
                                {settings_group_view(move || tr().imaging_exposure, EXPOSURE_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
                                {settings_group_view(move || tr().imaging_frame, FRAME_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
                                {settings_group_view(move || tr().imaging_gain_iso, GAIN_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
                                {settings_group_view(move || tr().imaging_filter, FILTER_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
                                {settings_group_view(move || tr().imaging_target, TARGET_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
                                {settings_group_view(move || tr().imaging_job_temperature, ENFORCE_TEMP_FIELDS, lang, camera, filter_wheel, get_setting, dispatch_setting.clone())}
                            </div>
                            <div class="flex justify-end gap-sp-2 py-sp-3 px-sp-4 border-t border-border-base bg-[rgba(10,12,20,0.8)]">
                                <button class=GHOST_BTN on:click=move |_| add_overlay_open.set(false)>{move || tr().cancel}</button>
                                <button class=ACTION_BTN style="--btn-color:var(--state-info);" on:click=move |_| {
                                    sv_add_job.with_value(|s| send_cmd(s, "capture_add_sequence", serde_json::json!({})));
                                    add_overlay_open.set(false);
                                }>{move || tr().imaging_add_exposure}</button>
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

fn marker_cls(open: bool) -> &'static str {
    let base = "inline-block w-[10px] text-xs text-[#557] transition-transform duration-[120ms]";
    if open {
        // Trick: prepend the rotation utility; keeping the base unchanged
        // means "▸" rotates 90° to act as the open chevron.
        // (Two leaked &str variants so the closure can return &'static str.)
        "inline-block w-[10px] text-xs text-[#557] transition-transform duration-[120ms] rotate-90"
    } else {
        base
    }
}

#[derive(Clone)]
struct SequenceRow {
    index: usize,
    completed: String,
    total: String,
    exp: String,
    ftype: String,
    filter: String,
    bin: String,
    status: String,
}

fn job_status_color(s: &str) -> &'static str {
    let lo = s.to_lowercase();
    if lo == "complete" {
        "var(--state-ok)"
    } else if lo == "capturing" || lo == "in progress" {
        "var(--state-info)"
    } else if lo.contains("abort") || lo.contains("error") {
        "var(--state-err)"
    } else {
        "var(--text-muted)"
    }
}

fn settings_group_view(
    title: impl Fn() -> &'static str + Copy + Send + Sync + 'static,
    fields: &'static [Field],
    lang: RwSignal<Lang>,
    camera: Signal<CameraSnapshot>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    get_value: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> leptos::prelude::AnyView {
    view! {
        <section class="border border-[#22263a] bg-[rgba(14,16,26,0.72)] rounded-sm p-sp-3">
            <h3 class="text-text-blue text-xs uppercase tracking-[0.08em] mb-sp-2">{title}</h3>
            {render_group(fields, lang, camera, filter_wheel, get_value, dispatch)}
        </section>
    }
    .into_any()
}

fn job_detail_rows(job: &serde_json::Value, t: &'static Translations) -> Vec<(String, String)> {
    let read = |key: &str| -> String { job_value_display(&job[key]) };
    vec![
        (t.status.to_string(), read("Status")),
        (t.field_frame_type.to_string(), read("Type")),
        (t.field_exposure_s.to_string(), read("Exp")),
        (t.field_count.to_string(), read("Count")),
        (t.field_filter.to_string(), read("Filter")),
        (t.field_bin_x.to_string(), read("Bin")),
        (t.field_gain.to_string(), read("ISO/Gain")),
        (t.field_offset.to_string(), read("Offset")),
        (t.field_encoding.to_string(), read("Encoding")),
        (t.field_format.to_string(), read("Format")),
        (t.field_job_temp_c.to_string(), read("Temperature")),
        (t.field_delay_s.to_string(), read("Delay")),
        (t.field_target_name.to_string(), read("Target")),
        (t.field_directory.to_string(), read("Directory")),
    ]
}

fn job_value_display(v: &serde_json::Value) -> String {
    let s = value_to_display(v);
    if s.is_empty() {
        "—".to_string()
    } else {
        s
    }
}

// ── Shared render helpers ────────────────────────────────────────────────

fn render_group(
    fields: &'static [Field],
    lang: RwSignal<Lang>,
    camera: Signal<CameraSnapshot>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    get_value: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> leptos::prelude::AnyView {
    view! {
        <div class="flex flex-col gap-[6px]">
            {fields.iter().map(|f| {
                let d = dispatch.clone();
                render_field(*f, lang, camera, filter_wheel, get_value, d)
            }).collect::<Vec<_>>()}
        </div>
    }
    .into_any()
}

fn render_field(
    field: Field,
    lang: RwSignal<Lang>,
    camera: Signal<CameraSnapshot>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    get_value: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> leptos::prelude::AnyView {
    // Use a reactive reader so the field updates as settings land.
    let current = move || get_value(field.key);

    let editor = match field.kind {
        Kind::Bool => {
            let d = dispatch.clone();
            view! {
                <input
                    type="checkbox"
                    prop:checked=move || current().as_bool().unwrap_or(false)
                    on:change=move |ev| {
                        d(field.key, serde_json::Value::Bool(event_target_checked(&ev)));
                    }
                />
            }
            .into_any()
        }
        Kind::Number => {
            let d = dispatch.clone();
            view! {
                <input
                    type="number"
                    prop:value=move || value_to_display(&current())
                    on:change=move |ev| {
                        let s = event_target_value(&ev);
                        if let Ok(n) = s.parse::<f64>() {
                            if let Some(num) = serde_json::Number::from_f64(n) {
                                d(field.key, serde_json::Value::Number(num));
                            }
                        }
                    }
                    class=FIELD_INPUT
                />
            }
            .into_any()
        }
        Kind::Text => {
            let d = dispatch.clone();
            view! {
                <input
                    type="text"
                    prop:value=move || value_to_display(&current())
                    on:change=move |ev| {
                        d(field.key, serde_json::Value::String(event_target_value(&ev)));
                    }
                    class=FIELD_INPUT
                />
            }
            .into_any()
        }
        Kind::ComboDynamic(get_opts) => {
            // Reactive option list: re-derived when camera/filter_wheel change.
            let opts_fn = move || get_opts(&camera.get(), &filter_wheel.get());
            render_select_dynamic(field.key, opts_fn, current, dispatch.clone())
        }
        Kind::ComboFilter(get_opts) => {
            let opts_fn = move || get_opts(&camera.get(), &filter_wheel.get());
            let placeholder = move || t(lang.get()).field_filter_none;
            render_select_filter(field.key, opts_fn, current, dispatch.clone(), placeholder)
        }
    };

    let label_fn = field.label;
    view! {
        <div class="flex items-center gap-sp-2 text-sm max-[479px]:flex-col max-[479px]:items-stretch max-[479px]:gap-[2px]">
            <span class=FIELD_LABEL>{move || label_fn(t(lang.get()))}</span>
            {editor}
        </div>
    }.into_any()
}

/// Render a `<select>` whose options are a fixed `Vec<String>`. If the
/// current value isn't in the list, prepend a disabled placeholder option
/// so we don't silently drop state.
fn render_select(
    key: &'static str,
    opts: Vec<String>,
    current: impl Fn() -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> leptos::prelude::AnyView {
    let d = dispatch.clone();
    let opts_for_render = opts.clone();
    view! {
        <select
            class=FIELD_INPUT
            prop:value=move || value_to_display(&current())
            on:change=move |ev| {
                d(key, serde_json::Value::String(event_target_value(&ev)));
            }
        >
            {move || {
                let cur = value_to_display(&current());
                let mut items: Vec<leptos::prelude::AnyView> = Vec::new();
                if !cur.is_empty() && !opts_for_render.iter().any(|o| o == &cur) {
                    let cur2 = cur.clone();
                    items.push(view! {
                        <option value=cur.clone() disabled=true selected=true>{cur2}</option>
                    }.into_any());
                }
                for o in &opts_for_render {
                    let v = o.clone();
                    let label = o.clone();
                    let is_sel = *o == cur;
                    items.push(view! {
                        <option value=v selected=is_sel>{label}</option>
                    }.into_any());
                }
                items
            }}
        </select>
    }
    .into_any()
}

/// Like `render_select`, but the option list is reactive — re-derived from
/// device snapshots on each render. Falls back to a free text input when
/// the option list is empty (e.g., no filter wheel attached).
fn render_select_dynamic(
    key: &'static str,
    opts_fn: impl Fn() -> Vec<String> + Copy + Send + Sync + 'static,
    current: impl Fn() -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> leptos::prelude::AnyView {
    let d_text = dispatch.clone();
    let d_select = dispatch.clone();
    view! {
        {move || {
            let opts = opts_fn();
            if opts.is_empty() {
                let d = d_text.clone();
                view! {
                    <input
                        type="text"
                        prop:value=move || value_to_display(&current())
                        on:change=move |ev| {
                            d(key, serde_json::Value::String(event_target_value(&ev)));
                        }
                        class=FIELD_INPUT
                    />
                }.into_any()
            } else {
                render_select(key, opts, current, d_select.clone())
            }
        }}
    }
    .into_any()
}

/// Filter-specific dropdown: always a `<select>`. When the option list is
/// empty, render a disabled select with a localized placeholder option —
/// no free-text fallback (filter values are never typed by hand).
fn render_select_filter(
    key: &'static str,
    opts_fn: impl Fn() -> Vec<String> + Copy + Send + Sync + 'static,
    current: impl Fn() -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
    placeholder: impl Fn() -> &'static str + Copy + Send + Sync + 'static,
) -> leptos::prelude::AnyView {
    let d_select = dispatch.clone();
    view! {
        {move || {
            let opts = opts_fn();
            if opts.is_empty() {
                view! {
                    <select class=FIELD_INPUT disabled=true>
                        <option selected=true>{placeholder()}</option>
                    </select>
                }.into_any()
            } else {
                render_select(key, opts, current, d_select.clone())
            }
        }}
    }
    .into_any()
}

fn value_to_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn default_capture_setting_value(key: &str) -> Option<serde_json::Value> {
    match key {
        // One-shot + sequence common defaults
        "captureExposureN" => serde_json::Number::from_f64(1.0).map(serde_json::Value::Number),
        "captureTypeS" => Some(serde_json::Value::String("Light".to_string())),
        "captureCountN" => Some(serde_json::Value::Number(1.into())),
        "captureDelayN" => Some(serde_json::Value::Number(0.into())),
        "captureBinHN" => Some(serde_json::Value::Number(1.into())),
        "captureBinVN" => Some(serde_json::Value::Number(1.into())),
        "captureGainN" => Some(serde_json::Value::Number(100.into())),
        "captureOffsetN" => Some(serde_json::Value::Number(0.into())),
        "cameraTemperatureEnforceB" => Some(serde_json::Value::Bool(false)),
        "cameraTemperatureN" => serde_json::Number::from_f64(-10.0).map(serde_json::Value::Number),
        _ => None,
    }
}

fn initial_preview_visible() -> bool {
    web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
        .map(|w| w >= 900.0)
        .unwrap_or(true)
}

fn capture_reveal_path(settings: &serde_json::Value) -> Option<String> {
    let dir = settings
        .get("fileDirectoryT")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if dir.is_empty() {
        None
    } else {
        Some(dir.to_string())
    }
}

fn event_target_checked(ev: &web_sys::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}

fn event_target_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}
