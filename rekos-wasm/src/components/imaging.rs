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

use crate::compat::{CameraSnapshot, CaptureSnapshot};
use crate::i18n::{Lang, Translations, t};
use crate::ws::SendCmd;
use crate::ws_helpers::{send_cmd, dispatch_setting as ws_dispatch_setting, send_device_property_set};

/// Responsive layout for the Imaging tab. Three breakpoints:
///   - wide  (>=1200px): preview | settings | sequence
///   - medium (>=760px, <1200px): preview spans top, settings | sequence share bottom
///   - narrow (<760px): stacked — preview, settings, sequence
const IMAGING_CSS: &str = r#"
.imaging-tab-root {
    position: absolute; inset: 0; background: #0a0a0f; color: #c0c0d0;
    font-family: monospace; display: grid; grid-template-rows: auto 1fr;
    overflow: hidden;
    -webkit-tap-highlight-color: rgba(136,170,255,0.25);
}
/* Kill the 300ms tap delay and prevent double-tap zoom on every tappable
   element inside the imaging tab. Inputs keep their native behaviour. */
.imaging-tab-root button,
.imaging-tab-root summary,
.imaging-tab-root .imaging-ghost-btn,
.imaging-tab-root .imaging-job-remove {
    touch-action: manipulation;
    -webkit-user-select: none;
    user-select: none;
}
.imaging-header {
    display: flex; flex-wrap: wrap; align-items: center; gap: 10px 18px;
    padding: 10px 20px 10px 80px;
    border-bottom: 1px solid #222; background: rgba(6,6,15,0.85);
    font-size: 13px; min-height: 44px;
}
.imaging-status-badge {
    display: inline-block; padding: 4px 10px; border-radius: 14px;
    font-size: 11px;
}
.imaging-stat { display: inline-flex; align-items: center; gap: 6px; }
.imaging-stat-label {
    color: #88aaff; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.06em;
}

/* Body: default wide layout */
.imaging-body {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 340px 320px;
    grid-template-rows: auto;
    min-height: 0;
    overflow-y: auto; -webkit-overflow-scrolling: touch;
    align-items: start;
}
/* Preview hidden — collapse its column */
.imaging-body.no-preview {
    grid-template-columns: minmax(0, 1fr) 320px;
}
.imaging-body.no-preview .imaging-preview { display: none; }
.imaging-body.no-preview .imaging-settings { border-right: 1px solid #222; }
.imaging-preview {
    position: sticky; top: 0;
    min-width: 0; height: 100%;
    overflow: hidden;
    display: flex; align-items: center; justify-content: center;
    background: #06060c; border-right: 1px solid #222;
}
.imaging-preview-img {
    max-width: 100%; max-height: 100%;
    object-fit: contain; image-rendering: pixelated;
}
.imaging-preview-empty {
    color: #444; font-size: 12px; text-align: center; padding: 0 12px;
}
.imaging-settings {
    display: flex; flex-direction: column;
    min-width: 0;
    padding: 14px; gap: 14px;
    border-right: 1px solid #222;
}
.imaging-sequence {
    position: sticky; top: 0;
    display: flex; flex-direction: column;
    min-width: 0; max-height: 100vh;
    overflow: hidden;
}
.imaging-sequence-head {
    display: flex; align-items: center; justify-content: space-between;
    gap: 8px; padding: 12px 14px 8px;
    border-bottom: 1px solid #222;
}
.imaging-sequence-title {
    color: #88aaff; font-size: 11px; text-transform: uppercase;
    letter-spacing: 0.08em;
}
.imaging-sequence-list {
    flex: 1; min-height: 0; overflow-y: auto; padding: 8px 10px;
}
.imaging-job-card {
    display: flex; flex-direction: column; gap: 3px;
    padding: 8px 10px; margin-bottom: 6px;
    background: rgba(14,16,26,0.85);
    border: 1px solid #22263a; border-radius: 4px;
}
.imaging-job-head {
    display: flex; align-items: center; gap: 8px;
}
.imaging-job-idx { color: #555; font-size: 10px; }
.imaging-job-main {
    flex: 1; color: #cfe0ff; font-size: 12px; font-weight: bold;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.imaging-job-meta {
    display: flex; flex-wrap: wrap; gap: 10px;
    color: #7a88a8; font-size: 10px;
}
.imaging-job-remove {
    background: transparent; border: 1px solid #443;
    color: #ff6a6a; padding: 2px 8px; cursor: pointer;
    font-family: monospace; font-size: 11px;
}

/* Foldable panels — native <details>/<summary> */
details.imaging-panel {
    border: 1px solid #222; background: rgba(10,12,20,0.55);
    border-radius: 3px; overflow: hidden;
}
details.imaging-panel > summary {
    list-style: none; cursor: pointer;
    padding: 8px 12px;
    color: #88aaff; font-size: 11px; font-weight: bold;
    text-transform: uppercase; letter-spacing: 0.08em;
    display: flex; align-items: center; gap: 8px;
    user-select: none;
}
details.imaging-panel > summary::-webkit-details-marker { display: none; }
details.imaging-panel > summary::before {
    content: "▸"; display: inline-block; width: 10px;
    font-size: 10px; color: #557;
    transition: transform 0.12s;
}
details.imaging-panel[open] > summary::before { transform: rotate(90deg); }
details.imaging-panel > summary:hover { background: rgba(20,24,40,0.7); }
details.imaging-panel > .imaging-panel-body {
    padding: 10px 12px 12px;
    border-top: 1px solid #1a1c28;
}

.imaging-job-badge {
    font-size: 9px; font-weight: bold; text-transform: uppercase;
    letter-spacing: 0.06em;
    padding: 1px 7px; border-radius: 3px;
    color: #0a0c14; white-space: nowrap;
}
.imaging-job-sep {
    color: #333; font-size: 10px;
}
.imaging-job-field {
    color: #aab8d0; font-size: 11px; white-space: nowrap;
}
.imaging-job-count {
    color: #cfe0ff; font-size: 11px; font-weight: bold; white-space: nowrap;
}

/* Medium: <=1200px — preview on top, settings + sequence side by side */
@media (max-width: 1199px) {
    .imaging-body {
        grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
        grid-template-rows: minmax(220px, 45%) auto;
    }
    .imaging-preview {
        grid-column: 1 / -1;
        position: relative; height: auto;
        border-right: none;
        border-bottom: 1px solid #222;
    }
    .imaging-settings { border-right: 1px solid #222; }
    .imaging-sequence {
        position: relative; max-height: none;
        overflow-y: auto;
    }
    .imaging-body.no-preview {
        grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
        grid-template-rows: auto;
    }
}

/* Narrow: <=760px — stacked single column, whole body scrolls */
@media (max-width: 759px) {
    .imaging-header {
        padding: 8px 12px;
        gap: 6px 12px;
        font-size: 12px;
    }
    .imaging-body {
        display: flex;
        flex-direction: column;
        overflow-y: auto;
        -webkit-overflow-scrolling: touch;
    }
    .imaging-preview {
        grid-column: auto;
        flex-shrink: 0;
        min-height: 200px;
        max-height: 40vh;
    }
    .imaging-settings {
        border-right: none;
        border-bottom: 1px solid #222;
        padding: 10px;
        gap: 10px;
        overflow-y: visible;
        flex-shrink: 0;
    }
    .imaging-sequence {
        flex-shrink: 0;
        overflow: visible;
    }
    .imaging-sequence-list {
        overflow-y: visible;
        max-height: none;
    }
    .imaging-body.no-preview .imaging-preview { display: none; }
}

/* Touch-first sizing: bigger tap targets + larger inputs on coarse
   pointers (phones, tablets). Applies regardless of width so a fine
   pointer on a narrow desktop window stays compact. */
@media (pointer: coarse) {
    .imaging-tab-root button,
    .imaging-tab-root .imaging-ghost-btn,
    .imaging-tab-root .imaging-job-remove {
        min-height: 44px;
        min-width: 44px;
        padding: 10px 14px;
        font-size: 13px;
    }
    .imaging-tab-root input[type="number"],
    .imaging-tab-root input[type="text"],
    .imaging-tab-root input[type="checkbox"] {
        min-height: 40px;
        font-size: 14px;
    }
    .imaging-tab-root input[type="checkbox"] {
        min-width: 24px;
        transform: scale(1.3);
        margin: 0 6px;
    }
    details.imaging-panel > summary {
        padding: 14px 14px;
        font-size: 12px;
    }
    .imaging-job-card {
        padding: 10px 12px;
    }
    .imaging-job-remove {
        min-height: 36px;
        min-width: 36px;
        padding: 4px 10px;
    }
    .imaging-status-badge {
        padding: 8px 14px;
    }
}

/* Settings toolbar + ghost button shared by header toggles */
.imaging-settings-toolbar {
    display: flex; align-items: center; justify-content: space-between;
    gap: 8px; padding-bottom: 6px; border-bottom: 1px solid #1a1c28;
    margin-bottom: 4px;
}
.imaging-settings-toolbar-title {
    color: #88aaff; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.08em;
}
.imaging-ghost-btn {
    background: rgba(12,14,24,0.9); border: 1px solid #334;
    color: #88aaff; padding: 4px 10px; cursor: pointer;
    font-family: monospace; font-size: 11px; border-radius: 3px;
}
.imaging-ghost-btn:hover { background: rgba(24,30,50,0.95); border-color: #88aaff; }
"#;

fn status_color(status: &str) -> &'static str {
    let s = status.to_lowercase();
    if s.contains("error") || s.contains("abort") || s.contains("fail") { "#ff6a6a" }
    else if s.contains("complete")  { "#7affa0" }
    else if s.contains("capturing") || s.contains("progress") { "#88aaff" }
    else if s.contains("image received") || s.contains("frame")  { "#88aaff" }
    else if s.contains("waiting") || s.contains("pause") { "#ffd060" }
    else { "#808090" }
}

/// Maps a KStars widget objectName to a human label. The widgets are defined
/// in `kstars/ekos/capture/camera.ui`; the keys are what comes back from
/// `capture_get_all_settings` and what `capture_set_all_settings` expects.
#[derive(Clone, Copy)]
enum Kind { Number, Text, Combo, Bool }

#[derive(Clone, Copy)]
struct Field {
    key:   &'static str,
    label: fn(&Translations) -> &'static str,
    kind:  Kind,
}

const EXPOSURE_FIELDS: &[Field] = &[
    Field { key: "captureExposureN", label: |t| t.field_exposure_s, kind: Kind::Number },
    Field { key: "captureTypeS",     label: |t| t.field_frame_type, kind: Kind::Combo  },
    Field { key: "captureCountN",    label: |t| t.field_count,      kind: Kind::Number },
    Field { key: "captureDelayN",    label: |t| t.field_delay_s,    kind: Kind::Number },
];

const FRAME_FIELDS: &[Field] = &[
    Field { key: "captureBinHN",    label: |t| t.field_bin_x,    kind: Kind::Number },
    Field { key: "captureBinVN",    label: |t| t.field_bin_y,    kind: Kind::Number },
    Field { key: "captureFormatS",  label: |t| t.field_format,   kind: Kind::Combo  },
    Field { key: "captureEncodingS",label: |t| t.field_encoding, kind: Kind::Combo  },
];

const GAIN_FIELDS: &[Field] = &[
    Field { key: "captureGainN",   label: |t| t.field_gain,   kind: Kind::Number },
    Field { key: "captureOffsetN", label: |t| t.field_offset, kind: Kind::Number },
    Field { key: "captureISOS",    label: |t| t.field_iso,    kind: Kind::Combo  },
];

const FILTER_FIELDS: &[Field] = &[
    Field { key: "FilterPosCombo", label: |t| t.field_filter, kind: Kind::Combo },
];

const TARGET_FIELDS: &[Field] = &[
    Field { key: "targetNameT",   label: |t| t.field_target_name, kind: Kind::Text },
    Field { key: "fileDirectoryT",label: |t| t.field_directory,   kind: Kind::Text },
];

const ENFORCE_TEMP_FIELDS: &[Field] = &[
    Field { key: "cameraTemperatureS", label: |t| t.field_enforce_temp, kind: Kind::Bool   },
    Field { key: "cameraTemperatureN", label: |t| t.field_job_temp_c,   kind: Kind::Number },
];

#[component]
pub fn ImagingTab(
    #[prop(into)] capture: Signal<CaptureSnapshot>,
    #[prop(into)] camera:  Signal<CameraSnapshot>,
    #[prop(into)] send:    SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let target_temp = RwSignal::new(-10.0_f64);

    // ── Collapsible-panel state ──────────────────────────────────────────
    // One signal per setting panel so "Collapse all" / "Expand all" can
    // drive them while each panel still behaves independently afterwards.
    let cooling_open  = RwSignal::new(true);
    let exposure_open = RwSignal::new(true);
    let frame_open    = RwSignal::new(true);
    let gain_open     = RwSignal::new(false);
    let filter_open   = RwSignal::new(true);
    let target_open   = RwSignal::new(false);
    let jobtemp_open  = RwSignal::new(false);
    let preview_visible = RwSignal::new(true);

    let all_panels = [
        cooling_open, exposure_open, frame_open, gain_open,
        filter_open,  target_open,   jobtemp_open,
    ];
    let all_collapse = all_panels;
    let on_collapse_all = move |_: web_sys::MouseEvent| {
        for s in all_collapse { s.set(false); }
    };
    let all_expand = all_panels;
    let on_expand_all = move |_: web_sys::MouseEvent| {
        for s in all_expand { s.set(true); }
    };
    let on_toggle_preview = move |_: web_sys::MouseEvent| {
        preview_visible.update(|v| *v = !*v);
    };

    // ── Action dispatchers ────────────────────────────────────────────────
    let s_start   = send.clone();
    let on_start   = move |_| send_cmd(&s_start, "capture_start",   serde_json::json!({}));
    let s_stop    = send.clone();
    let on_stop    = move |_| send_cmd(&s_stop,  "capture_stop",    serde_json::json!({}));
    let s_preview = send.clone();
    let on_preview = move |_| send_cmd(&s_preview,"capture_preview", serde_json::json!({}));
    let s_loop    = send.clone();
    let on_loop    = move |_| send_cmd(&s_loop,  "capture_loop",    serde_json::json!({}));

    let s_add   = send.clone();
    let on_add_job   = move |_| send_cmd(&s_add,   "capture_add_sequence",    serde_json::json!({}));
    let s_clear = send.clone();
    let on_clear_seq = move |_| send_cmd(&s_clear, "capture_clear_sequences", serde_json::json!({}));

    // ── Save / Load sequence file ─────────────────────────────────────────
    // StoredValue is Copy, so these can be captured by on:click closures inside
    // a <Show> children-closure without turning it FnOnce.
    let save_open = RwSignal::new(false);
    let save_path = RwSignal::new(String::new());
    let sv_save   = StoredValue::new(send.clone());

    let load_open = RwSignal::new(false);
    let load_path = RwSignal::new(String::new());
    let sv_load   = StoredValue::new(send.clone());

    // ── Cooling → INDI device_property_set on the active camera ───────────
    let s_cool_on = send.clone();
    let cam_cool_on = camera;
    let on_cooler_on = move |_| {
        let dev = cam_cool_on.with(|c| c.device.clone());
        if dev.is_empty() { return; }
        send_device_property_set(&s_cool_on, &dev, "CCD_COOLER", serde_json::json!([
            { "name": "COOLER_ON",  "state": 1 },
            { "name": "COOLER_OFF", "state": 0 },
        ]));
    };
    let s_cool_off = send.clone();
    let cam_cool_off = camera;
    let on_cooler_off = move |_| {
        let dev = cam_cool_off.with(|c| c.device.clone());
        if dev.is_empty() { return; }
        send_device_property_set(&s_cool_off, &dev, "CCD_COOLER", serde_json::json!([
            { "name": "COOLER_ON",  "state": 0 },
            { "name": "COOLER_OFF", "state": 1 },
        ]));
    };
    let s_set_temp = send.clone();
    let cam_set_temp = camera;
    let on_set_temp = move |_| {
        let dev = cam_set_temp.with(|c| c.device.clone());
        if dev.is_empty() { return; }
        send_device_property_set(&s_set_temp, &dev, "CCD_TEMPERATURE", serde_json::json!([
            { "name": "CCD_TEMPERATURE_VALUE", "value": target_temp.get() },
        ]));
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
        let Some(arr) = seq.as_array() else { return Vec::new(); };
        // Live frame count from new_capture_state (seqv / seqr)
        let live_current = cap.seq_current;
        let live_total   = cap.seq_total;
        arr.iter().enumerate().map(|(i, job)| {
            let count_raw = job["Count"].as_str().unwrap_or("0/0");
            let (completed, total) = match count_raw.split_once('/') {
                Some((c, t)) => (c.trim().to_string(), t.trim().to_string()),
                None         => (String::new(), count_raw.to_string()),
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
            let exp    = job["Exp"].as_str().unwrap_or("—").to_string();
            let ftype  = job["Type"].as_str().unwrap_or("").to_string();
            let filter = job["Filter"].as_str().unwrap_or("").to_string();
            let bin    = job["Bin"].as_str().unwrap_or("").to_string();
            SequenceRow { index: i, completed, total, exp, ftype, filter, bin, status }
        }).collect::<Vec<_>>()
    };

    let s_remove = send.clone();
    let on_remove_job = move |idx: usize| {
        send_cmd(&s_remove, "capture_remove_sequence", serde_json::json!({ "index": idx }));
    };

    // Shared setting lookup: returns the current Value from the debounced
    // capture_get_all_settings snapshot, or Null.
    let get_setting = move |key: &'static str| -> serde_json::Value {
        capture.with(|c| {
            c.settings.as_object()
                .and_then(|o| o.get(key).cloned())
                .unwrap_or(serde_json::Value::Null)
        })
    };

    view! {
        <div class="imaging-tab-root">
            <style>{IMAGING_CSS}</style>

            // ── Header ────────────────────────────────────────────────────
            <div class="imaging-header">
                <span class="imaging-status-badge" style=move || format!(
                    "border:1px solid {c}; color:{c};",
                    c = status_color(&capture.with(|c| c.status.clone()))
                )>
                    {move || {
                        let s = capture.with(|c| c.status.clone());
                        if s.is_empty() { tr().idle.to_string() } else { s }
                    }}
                </span>
                <span class="imaging-stat">
                    <span class="imaging-stat-label">{move || tr().imaging_camera}</span>
                    <span>{move || {
                        let d = camera.with(|c| c.device.clone());
                        if d.is_empty() { "—".to_string() } else { d }
                    }}</span>
                </span>
                <span class="imaging-stat">
                    <span class="imaging-stat-label">{move || tr().imaging_temp}</span>
                    <span>{move || camera.with(|c| c.temperature
                        .map(|v| format!("{:.1}°C", v))
                        .unwrap_or_else(|| "—".into()))}</span>
                </span>
                <span class="imaging-stat">
                    <span class="imaging-stat-label">{move || tr().imaging_cooler}</span>
                    <span style=move || {
                        let on = camera.with(|c| c.cooler_on).unwrap_or(false);
                        format!("color:{};", if on { "#7affa0" } else { "#808090" })
                    }>{move || match camera.with(|c| c.cooler_on) {
                        Some(true)  => tr().imaging_cooler_on_val.to_string(),
                        Some(false) => tr().imaging_cooler_off_val.to_string(),
                        None        => "—".to_string(),
                    }}</span>
                </span>
                <span class="imaging-stat">
                    <span class="imaging-stat-label">{move || tr().imaging_sensor}</span>
                    <span>{move || camera.with(|c| match (c.sensor_width, c.sensor_height) {
                        (Some(w), Some(h)) => format!("{}×{}", w, h),
                        _ => "—".into(),
                    })}</span>
                </span>
                <span class="imaging-stat">
                    <span class="imaging-stat-label">{move || tr().imaging_progress}</span>
                    <span>{move || capture.with(|c| match (c.seq_current, c.seq_total) {
                        (Some(a), Some(b)) => format!("{} / {}", a, b),
                        _ => "—".into(),
                    })}</span>
                </span>
                <button
                    class="imaging-ghost-btn"
                    style="margin-left:auto;"
                    on:click=on_toggle_preview
                    title=move || tr().imaging_toggle_preview_title>
                    {move || if preview_visible.get() { tr().imaging_hide_preview } else { tr().imaging_show_preview }}
                </button>
            </div>

            // ── Body: responsive grid ─────────────────────────────────────
            <div class=move || if preview_visible.get() { "imaging-body" } else { "imaging-body no-preview" }>
                // ─ Preview ────────────────────────────────────────────────
                <div class="imaging-preview">
                    {move || match capture.with(|c| c.preview_url.clone()) {
                        Some(url) => view! {
                            <img class="imaging-preview-img" src=url />
                        }.into_any(),
                        None => view! {
                            <div class="imaging-preview-empty">
                                {move || tr().imaging_no_frame}
                            </div>
                        }.into_any(),
                    }}
                </div>

                // ─ Settings ──────────────────────────────────────────────
                <div class="imaging-settings">

                    // Toolbar: collapse / expand all panels
                    <div class="imaging-settings-toolbar">
                        <span class="imaging-settings-toolbar-title">{move || tr().imaging_capture_controls}</span>
                        <div style="display:flex; gap:6px;">
                            <button class="imaging-ghost-btn" on:click=on_collapse_all>{move || tr().imaging_collapse_all}</button>
                            <button class="imaging-ghost-btn" on:click=on_expand_all>{move || tr().imaging_expand_all}</button>
                        </div>
                    </div>

                    // Actions — always visible, not foldable
                    <fieldset style=card_style()>
                        <legend style=legend_style()>{move || tr().imaging_actions}</legend>
                        <div style="display:grid; grid-template-columns:1fr 1fr; gap:8px;">
                            <button on:click=on_start   style=action_btn("#7affa0")>{move || tr().start}</button>
                            <button on:click=on_stop    style=action_btn("#ff6a6a")>{move || tr().stop}</button>
                            <button on:click=on_preview style=action_btn("#88aaff")>{move || tr().preview}</button>
                            <button on:click=on_loop    style=action_btn("#88aaff")>{move || tr().focus_loop_btn}</button>
                        </div>
                    </fieldset>

                    // Cooling — foldable, open by default
                    <details
                        class="imaging-panel"
                        prop:open=move || cooling_open.get()
                        on:toggle=move |ev: web_sys::Event| {
                            if let Some(el) = ev.target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                            {
                                cooling_open.set(el.open());
                            }
                        }>
                        <summary>{move || tr().imaging_cooling}</summary>
                        <div class="imaging-panel-body">
                            <div style="display:flex; align-items:center; gap:8px; margin-bottom:8px;">
                                <span style=label_style()>{move || tr().imaging_target_c}</span>
                                <input
                                    type="number"
                                    step="0.5"
                                    value=move || format!("{:.1}", target_temp.get())
                                    on:change=move |ev| {
                                        let s = event_target_value(&ev);
                                        if let Ok(n) = s.parse::<f64>() { target_temp.set(n); }
                                    }
                                    style=input_style()
                                />
                                <button on:click=on_set_temp style=action_btn("#88aaff")>{move || tr().imaging_set}</button>
                            </div>
                            <div style="display:grid; grid-template-columns:1fr 1fr; gap:8px;">
                                <button on:click=on_cooler_on  style=action_btn("#7affa0")>{move || tr().cooler_on}</button>
                                <button on:click=on_cooler_off style=action_btn("#ff6a6a")>{move || tr().cooler_off}</button>
                            </div>
                        </div>
                    </details>

                    <details
                        class="imaging-panel"
                        prop:open=move || exposure_open.get()
                        on:toggle=move |ev: web_sys::Event| {
                            if let Some(el) = ev.target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                            { exposure_open.set(el.open()); }
                        }>
                        <summary>{move || tr().imaging_exposure}</summary>
                        <div class="imaging-panel-body">
                            {render_group(EXPOSURE_FIELDS, lang, get_setting, dispatch_setting.clone())}
                        </div>
                    </details>

                    <details
                        class="imaging-panel"
                        prop:open=move || frame_open.get()
                        on:toggle=move |ev: web_sys::Event| {
                            if let Some(el) = ev.target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                            { frame_open.set(el.open()); }
                        }>
                        <summary>{move || tr().imaging_frame}</summary>
                        <div class="imaging-panel-body">
                            {render_group(FRAME_FIELDS, lang, get_setting, dispatch_setting.clone())}
                        </div>
                    </details>

                    <details
                        class="imaging-panel"
                        prop:open=move || gain_open.get()
                        on:toggle=move |ev: web_sys::Event| {
                            if let Some(el) = ev.target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                            { gain_open.set(el.open()); }
                        }>
                        <summary>{move || tr().imaging_gain_iso}</summary>
                        <div class="imaging-panel-body">
                            {render_group(GAIN_FIELDS, lang, get_setting, dispatch_setting.clone())}
                        </div>
                    </details>

                    <details
                        class="imaging-panel"
                        prop:open=move || filter_open.get()
                        on:toggle=move |ev: web_sys::Event| {
                            if let Some(el) = ev.target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                            { filter_open.set(el.open()); }
                        }>
                        <summary>{move || tr().imaging_filter}</summary>
                        <div class="imaging-panel-body">
                            {render_group(FILTER_FIELDS, lang, get_setting, dispatch_setting.clone())}
                        </div>
                    </details>

                    <details
                        class="imaging-panel"
                        prop:open=move || target_open.get()
                        on:toggle=move |ev: web_sys::Event| {
                            if let Some(el) = ev.target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                            { target_open.set(el.open()); }
                        }>
                        <summary>{move || tr().imaging_target}</summary>
                        <div class="imaging-panel-body">
                            {render_group(TARGET_FIELDS, lang, get_setting, dispatch_setting.clone())}
                        </div>
                    </details>

                    <details
                        class="imaging-panel"
                        prop:open=move || jobtemp_open.get()
                        on:toggle=move |ev: web_sys::Event| {
                            if let Some(el) = ev.target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok())
                            { jobtemp_open.set(el.open()); }
                        }>
                        <summary>{move || tr().imaging_job_temperature}</summary>
                        <div class="imaging-panel-body">
                            {render_group(ENFORCE_TEMP_FIELDS, lang, get_setting, dispatch_setting.clone())}
                        </div>
                    </details>
                </div>

                // ─ Sequence queue ────────────────────────────────────────
                <div class="imaging-sequence">
                    <div class="imaging-sequence-head">
                        <span class="imaging-sequence-title">{move || tr().imaging_sequence_queue}</span>
                        <div style="display:flex; gap:6px;">
                            <button on:click=on_add_job   style=action_btn("#88aaff")>{move || tr().imaging_add_job}</button>
                            <button on:click=on_clear_seq style=action_btn("#ff6a6a")>{move || tr().seq_clear}</button>
                            <button
                                style=action_btn("#aaffcc")
                                on:click=move |_| { save_open.update(|v| *v = !*v); load_open.set(false); }>
                                {move || tr().save_profile}
                            </button>
                            <button
                                style=action_btn("#ffcc88")
                                on:click=move |_| { load_open.update(|v| *v = !*v); save_open.set(false); }>
                                {move || tr().load_profile}
                            </button>
                        </div>
                    </div>
                    // Save inline row
                    <Show when=move || save_open.get()>
                        <div style="display:flex; gap:6px; padding:6px 8px; background:#0d1a12; border-bottom:1px solid #224433;">
                            <input
                                type="text"
                                placeholder="/home/user/seq.esq"
                                prop:value=move || save_path.get()
                                on:input=move |ev| save_path.set(event_target_value(&ev))
                                style="flex:1; background:#111; color:#c0ffd0; border:1px solid #335544; padding:4px 8px; font-family:monospace; font-size:12px;"
                            />
                            <button style=action_btn("#aaffcc") on:click=move |_| {
                                let path = save_path.get_untracked();
                                if !path.is_empty() {
                                    sv_save.with_value(|s| send_cmd(s, "capture_save_sequence_file", serde_json::json!({"filepath": path})));
                                    save_open.set(false);
                                }
                            }>"✓"</button>
                            <button style=action_btn("#555") on:click=move |_| save_open.set(false)>"✕"</button>
                        </div>
                    </Show>
                    // Load inline row
                    <Show when=move || load_open.get()>
                        <div style="display:flex; gap:6px; padding:6px 8px; background:#1a1200; border-bottom:1px solid #443322;">
                            <input
                                type="text"
                                placeholder="/home/user/seq.esq"
                                prop:value=move || load_path.get()
                                on:input=move |ev| load_path.set(event_target_value(&ev))
                                style="flex:1; background:#111; color:#ffd0aa; border:1px solid #554433; padding:4px 8px; font-family:monospace; font-size:12px;"
                            />
                            <button style=action_btn("#ffcc88") on:click=move |_| {
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
                            <button style=action_btn("#555") on:click=move |_| load_open.set(false)>"✕"</button>
                        </div>
                    </Show>
                    <div class="imaging-sequence-list">
                        {move || {
                            let rows = sequence_rows();
                            if rows.is_empty() {
                                return view! {
                                    <div style="color:#555; font-size:11px; padding:12px 6px;">
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
                                    <div class="imaging-job-card">
                                        <div class="imaging-job-head">
                                            <span class="imaging-job-idx">{format!("#{}", idx + 1)}</span>
                                            <span
                                                class="imaging-job-badge"
                                                style:background=badge_color>
                                                {r.status}
                                            </span>
                                            <button
                                                class="imaging-job-remove"
                                                title=tr().imaging_remove_job
                                                on:click=move |_| on_remove(idx)>
                                                "×"
                                            </button>
                                        </div>
                                        <div class="imaging-job-meta">
                                            <span class="imaging-job-field">{r.ftype}</span>
                                            <span class="imaging-job-sep">"|"</span>
                                            <span class="imaging-job-field">{format!("{} s", r.exp)}</span>
                                            <span class="imaging-job-sep">"|"</span>
                                            <span class="imaging-job-field">{filter_label}</span>
                                            <span class="imaging-job-sep">"|"</span>
                                            <span class="imaging-job-count">
                                                {format!("{} / {}", r.completed, r.total)}
                                            </span>
                                        </div>
                                    </div>
                                }.into_any()
                            }).collect::<Vec<_>>().into_any()
                        }}
                    </div>
                </div>
            </div>
        </div>
    }
}

#[derive(Clone)]
struct SequenceRow {
    index:     usize,
    completed: String,
    total:     String,
    exp:       String,
    ftype:     String,
    filter:    String,
    bin:       String,
    status:    String,
}

fn job_status_color(s: &str) -> &'static str {
    let lo = s.to_lowercase();
    if lo == "complete"                                     { "#7affa0" }
    else if lo == "capturing" || lo == "in progress"        { "#88aaff" }
    else if lo.contains("abort") || lo.contains("error")   { "#ff6a6a" }
    else                                                    { "#808090" }
}

// ── Shared render helpers ────────────────────────────────────────────────

fn render_group(
    fields: &'static [Field],
    lang: RwSignal<Lang>,
    get_value: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> leptos::prelude::AnyView {
    view! {
        <div style="display:flex; flex-direction:column; gap:6px;">
            {fields.iter().map(|f| {
                let d = dispatch.clone();
                render_field(*f, lang, get_value, d)
            }).collect::<Vec<_>>()}
        </div>
    }.into_any()
}

fn render_field(
    field: Field,
    lang: RwSignal<Lang>,
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
            }.into_any()
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
                    style=input_style()
                />
            }.into_any()
        }
        Kind::Text | Kind::Combo => {
            let d = dispatch.clone();
            view! {
                <input
                    type="text"
                    prop:value=move || value_to_display(&current())
                    on:change=move |ev| {
                        d(field.key, serde_json::Value::String(event_target_value(&ev)));
                    }
                    style=input_style()
                />
            }.into_any()
        }
    };

    let label_fn = field.label;
    view! {
        <div style="display:flex; align-items:center; gap:8px; font-size:11px;">
            <span style=label_style()>{move || label_fn(t(lang.get()))}</span>
            {editor}
        </div>
    }.into_any()
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

// ── Style helpers ────────────────────────────────────────────────────────

fn card_style() -> &'static str {
    "border:1px solid #222; padding:10px 12px;"
}

fn legend_style() -> &'static str {
    "color:#88aaff; padding:0 6px; font-size:11px; text-transform:uppercase; letter-spacing:0.06em;"
}

fn label_style() -> &'static str {
    "flex:0 0 120px; color:#88aaff; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;"
}

fn action_btn(color: &str) -> String {
    format!(
        "padding:8px 10px; background:rgba(12,14,24,0.9); \
         border:1px solid {c}; color:{c}; cursor:pointer; \
         font-family:monospace; font-size:12px;",
        c = color
    )
}

fn input_style() -> &'static str {
    "flex:1; min-width:0; background:#06060c; color:#cfe0ff; border:1px solid #222; \
     padding:4px 6px; font-family:monospace; font-size:12px;"
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
