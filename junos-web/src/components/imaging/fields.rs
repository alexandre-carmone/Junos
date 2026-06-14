//! Shared field-rendering helpers for the Imaging tab — the row layouts,
//! exposure widget, filter dropdown, frame-type segmented control, and the
//! generic `<select>` machinery used by them.

use leptos::prelude::*;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot};
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_device_property_set;

use super::styles::{frame_type_visual, FIELD_INPUT, FIELD_LABEL, FRAME_TYPE_FALLBACK};
use super::types::{Field, Kind, EXPOSURE_PRESETS};
use super::util::event_target_value;

#[allow(dead_code)]
pub(super) fn render_group(
    fields: &'static [Field],
    lang: RwSignal<Lang>,
    camera: Signal<CameraSnapshot>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    get_value: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> AnyView {
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

pub(super) fn render_field(
    field: Field,
    lang: RwSignal<Lang>,
    camera: Signal<CameraSnapshot>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    get_value: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> AnyView {
    // Use a reactive reader so the field updates as settings land.
    let current = move || get_value(field.key);

    let editor = match field.kind {
        Kind::Number => {
            let d = dispatch.clone();
            let is_int = matches!(field.key, "captureGainN" | "captureOffsetN");
            let min_attr = if is_int { Some("0") } else { None };
            let step_attr = if is_int { Some("1") } else { None };
            view! {
                <input
                    type="number"
                    min=min_attr
                    step=step_attr
                    prop:value=move || value_to_display(&current())
                    on:change=move |ev| {
                        let s = event_target_value(&ev);
                        if is_int {
                            if let Ok(n) = s.parse::<i64>() {
                                d(field.key, serde_json::Value::Number(n.into()));
                            }
                        } else if let Ok(n) = s.parse::<f64>() {
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

/// Like `render_field`, but with the label stacked above the editor so the
/// editor gets the full column width. Used for the Filter / Gain / ISO trio
/// in the redesigned one-shot panel where each grid column is too narrow
/// for the default `120px label + flex-1 input` row to be usable.
pub(super) fn render_stacked_field(
    field: Field,
    lang: RwSignal<Lang>,
    camera: Signal<CameraSnapshot>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    get_value: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> AnyView {
    let current = move || get_value(field.key);
    let editor = match field.kind {
        Kind::Number => {
            let d = dispatch.clone();
            let is_int = matches!(field.key, "captureGainN" | "captureOffsetN");
            let min_attr = if is_int { Some("0") } else { None };
            let step_attr = if is_int { Some("1") } else { None };
            view! {
                <input
                    type="number"
                    min=min_attr
                    step=step_attr
                    prop:value=move || value_to_display(&current())
                    on:change=move |ev| {
                        let s = event_target_value(&ev);
                        if is_int {
                            if let Ok(n) = s.parse::<i64>() {
                                d(field.key, serde_json::Value::Number(n.into()));
                            }
                        } else if let Ok(n) = s.parse::<f64>() {
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
        Kind::ComboDynamic(get_opts) => {
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
        <div class="flex flex-col gap-[3px] text-sm min-w-0">
            <span class="text-text-blue text-xs uppercase tracking-[0.06em] overflow-hidden text-ellipsis whitespace-nowrap">
                {move || label_fn(t(lang.get()))}
            </span>
            {editor}
        </div>
    }.into_any()
}

/// Bespoke editor for `captureExposureN`. KStars accepts 0.001s–3600s with
/// 3 decimals (camera.ui:752); the generic Number editor only set step=1
/// for non-int keys, so the browser silently rejected decimal exposures.
/// Here `step="any"` + `inputmode="decimal"` accepts any positive number,
/// `on:input` is debounced ~250 ms so we don't flood KStars per keystroke,
/// and a chip row provides one-tap presets for common exposure lengths.
pub(super) fn render_exposure_field(
    lang: RwSignal<Lang>,
    get_setting: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> AnyView {
    let get_value = move || get_setting("captureExposureN");
    let dispatch_input = dispatch.clone();
    // gloo Timeout is !Send/!Sync, so use the LocalStorage variant.
    let debounce: StoredValue<Option<gloo_timers::callback::Timeout>, LocalStorage> =
        StoredValue::new_local(None);
    let dispatch_chip = dispatch.clone();
    // While the user is actively typing, `editing` holds the raw text so the
    // controlled `prop:value` echoes it verbatim instead of snapping back to the
    // stale server value during the 250 ms debounce window. Cleared once the
    // debounced dispatch commits (or a preset chip overrides the field), after
    // which `get_value()` drives the display again.
    let editing: RwSignal<Option<String>> = RwSignal::new(None);

    view! {
        <div class="flex flex-col gap-sp-2">
            <div class="flex items-center gap-sp-2 text-sm max-[479px]:flex-col max-[479px]:items-stretch max-[479px]:gap-[2px]">
                <span class=FIELD_LABEL>{move || t(lang.get()).field_exposure_s}</span>
                <div class="relative flex-1 min-w-0">
                    <input
                        type="number"
                        min="0.001"
                        max="3600"
                        step="any"
                        inputmode="decimal"
                        prop:value=move || {
                            editing.get().unwrap_or_else(|| value_to_display(&get_value()))
                        }
                        on:input=move |ev| {
                            let raw = event_target_value(&ev);
                            // Pin the typed text immediately so the controlled
                            // value doesn't revert while the dispatch debounces.
                            editing.set(Some(raw.clone()));
                            let d = dispatch_input.clone();
                            // Cancel any pending dispatch and queue a fresh one.
                            // Dropping the previous Timeout cancels it.
                            let timeout = gloo_timers::callback::Timeout::new(250, move || {
                                let trimmed = raw.trim();
                                if trimmed.is_empty() { return; }
                                let Ok(n) = trimmed.parse::<f64>() else { return; };
                                if !n.is_finite() || n <= 0.0 { return; }
                                if let Some(num) = serde_json::Number::from_f64(n) {
                                    d("captureExposureN", serde_json::Value::Number(num));
                                    // Hand display back to the committed value.
                                    editing.set(None);
                                }
                            });
                            debounce.set_value(Some(timeout));
                        }
                        class=format!("{FIELD_INPUT} pr-[22px] text-base font-bold")
                    />
                    <span class="pointer-events-none absolute right-sp-2 top-1/2 -translate-y-1/2 text-text-blue text-xs">"s"</span>
                </div>
            </div>
            <div class="flex flex-wrap gap-[6px] pl-[120px] max-[479px]:pl-0">
                {EXPOSURE_PRESETS.iter().copied().map(|preset| {
                    let d = dispatch_chip.clone();
                    let label = format_preset(preset);
                    let active = move || get_value().as_f64()
                        .map(|n| (n - preset).abs() < 1e-6)
                        .unwrap_or(false);
                    view! {
                        <button
                            type="button"
                            class="btn btn--sm font-mono"
                            class:btn-ghost=move || !active()
                            class:text-text-blue=move || !active()
                            style=move || if active() {
                                "border-color:var(--state-info);color:var(--state-info);background:rgba(40,80,140,0.18);"
                            } else { "" }
                            on:click=move |_| {
                                // Cancel any in-flight debounced text dispatch
                                // and push the preset value immediately.
                                debounce.set_value(None);
                                editing.set(None);
                                if let Some(num) = serde_json::Number::from_f64(preset) {
                                    d("captureExposureN", serde_json::Value::Number(num));
                                }
                            }
                        >{label}</button>
                    }.into_any()
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }.into_any()
}

fn format_preset(s: f64) -> String {
    if s >= 1.0 {
        format!("{}s", s as i64)
    } else if s >= 0.01 {
        // 0.01, 0.1 → strip trailing zeros for readability
        let mut t = format!("{:.2}", s);
        while t.ends_with('0') { t.pop(); }
        if t.ends_with('.') { t.pop(); }
        format!("{}s", t)
    } else {
        // 0.001 → "1ms" reads better than "0.001s"
        format!("{}ms", (s * 1000.0).round() as i64)
    }
}

/// Filter dropdown that actually moves the wheel. Changing FilterPosCombo
/// via `capture_set_all_settings` only updates KStars's combo state — the
/// physical wheel is driven by the FilterWheel device's INDI FILTER_SLOT
/// number property (1-based slot). We do both: dispatch the capture-side
/// setting (so the sequence-job UI stays consistent) and also push
/// FILTER_SLOT to the device so the wheel rotates.
pub(super) fn render_filter_field(
    lang: RwSignal<Lang>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    get_setting: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
    send: SendCmd,
) -> AnyView {
    let current_text = move || value_to_display(&get_setting("FilterPosCombo"));

    view! {
        <div class="flex flex-col gap-[3px] text-sm min-w-0">
            <span class="text-text-blue text-xs uppercase tracking-[0.06em] overflow-hidden text-ellipsis whitespace-nowrap">
                {move || t(lang.get()).field_filter}
            </span>
            {move || {
                let names = filter_wheel.with(|fw| fw.filter_names.clone());
                if names.is_empty() {
                    let placeholder = t(lang.get()).field_filter_none;
                    view! {
                        <select class=FIELD_INPUT disabled=true>
                            <option selected=true>{placeholder}</option>
                        </select>
                    }.into_any()
                } else {
                    let cur = current_text();
                    let dispatch = dispatch.clone();
                    let send = send.clone();
                    let fw_dev = filter_wheel.with(|fw| fw.device.clone());
                    let names_for_options = names.clone();
                    view! {
                        <select
                            class=FIELD_INPUT
                            on:change=move |ev| {
                                let picked = event_target_value(&ev);
                                if picked.is_empty() { return; }
                                // Capture-side setting (keeps preview / job UI in sync).
                                dispatch(
                                    "FilterPosCombo",
                                    serde_json::Value::String(picked.clone()),
                                );
                                // Physical move: 1-based slot index in FILTER_SLOT.
                                if !fw_dev.is_empty() {
                                    if let Some(idx) = names.iter().position(|n| n == &picked) {
                                        send_device_property_set(
                                            &send,
                                            &fw_dev,
                                            "FILTER_SLOT",
                                            serde_json::json!([
                                                { "name": "FILTER_SLOT_VALUE", "value": (idx + 1) as i64 },
                                            ]),
                                        );
                                    }
                                }
                            }
                        >
                            {names_for_options.into_iter().map(|n| {
                                let v = n.clone();
                                let label = n.clone();
                                let v_for_sel = n;
                                let cur_for_sel = cur.clone();
                                view! {
                                    <option
                                        value=v
                                        selected=cur_for_sel == v_for_sel
                                    >{label}</option>
                                }.into_any()
                            }).collect::<Vec<_>>()}
                        </select>
                    }.into_any()
                }
            }}
        </div>
    }.into_any()
}

/// Frame-type segmented control. Source list still comes from the camera
/// snapshot (with `FRAME_TYPE_FALLBACK` until the device reports), and
/// click dispatches `captureTypeS` exactly like the previous combo did.
pub(super) fn render_frame_type_segmented(
    _lang: RwSignal<Lang>,
    camera: Signal<CameraSnapshot>,
    get_setting: impl Fn(&'static str) -> serde_json::Value + Copy + Send + Sync + 'static,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + Send + Sync + 'static,
) -> AnyView {
    let current = move || value_to_display(&get_setting("captureTypeS"));
    let dispatch = std::sync::Arc::new(dispatch);

    view! {
        // Row of color-coded icon pills. The "Frame Type" label is dropped:
        // the icons + colors + their position in the one-shot panel make
        // the role of the control obvious, and reclaiming the 120px label
        // column lets the pills breathe. On phones we switch to a 2×2 grid
        // (instead of stacking 4 deep) to keep the panel compact.
        <div class="grid grid-cols-4 gap-sp-2 max-[479px]:grid-cols-2">
            {move || {
                let opts = camera.with(|c| if c.frame_type_options.is_empty() {
                    FRAME_TYPE_FALLBACK.iter().map(|s| s.to_string()).collect()
                } else {
                    c.frame_type_options.clone()
                });
                opts.into_iter().map(|opt| {
                    let (icon, color) = frame_type_visual(&opt);
                    let opt_for_active_a = opt.clone();
                    let opt_for_active_b = opt.clone();
                    let active_pill = move || current() == opt_for_active_a;
                    let active_icon = move || current() == opt_for_active_b;
                    let opt_for_dispatch = opt.clone();
                    let d = dispatch.clone();
                    // Active pill: filled tint of its own color + ring;
                    // inactive: muted border, dim icon, blue label.
                    let pill_style = move || if active_pill() {
                        format!(
                            "background:color-mix(in srgb, {c} 22%, transparent);\
                             border-color:{c};color:{c};\
                             box-shadow:inset 0 0 0 1px {c};",
                            c = color,
                        )
                    } else {
                        format!(
                            "background:transparent;\
                             border-color:var(--border-base);\
                             color:var(--text-blue);",
                        )
                    };
                    let icon_style = move || if active_icon() {
                        format!("color:{color};opacity:1;")
                    } else {
                        format!("color:{color};opacity:0.6;")
                    };
                    view! {
                        <button
                            type="button"
                            class="flex items-center justify-center gap-[6px] min-w-0 h-[32px] px-sp-2 \
                                   rounded-[6px] border text-xs uppercase tracking-[0.06em] \
                                   font-medium transition-colors \
                                   hover:bg-[rgba(255,255,255,0.04)] \
                                   focus:outline-none focus:ring-1 focus:ring-offset-0"
                            style=pill_style
                            on:click=move |_| {
                                d("captureTypeS",
                                  serde_json::Value::String(opt_for_dispatch.clone()));
                            }
                        >
                            <span
                                class="inline-flex shrink-0 transition-opacity"
                                style=icon_style
                                inner_html=icon
                            />
                            <span class="truncate">{opt}</span>
                        </button>
                    }.into_any()
                }).collect::<Vec<_>>()
            }}
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
) -> AnyView {
    let d = dispatch.clone();
    // Build options once. Mark `selected` reactively per-option so we don't
    // tear down the option list every time `current` changes — that previously
    // left the <select> showing nothing after an edit, because option nodes
    // were swapped out from under `prop:value`.
    let opts_unknown = opts.clone();
    let unknown_opt = {
        let current_a = current;
        let opts_a = opts_unknown.clone();
        move || {
            let cur = value_to_display(&current_a());
            if !cur.is_empty() && !opts_a.iter().any(|o| o == &cur) {
                let cur2 = cur.clone();
                Some(view! {
                    <option value=cur.clone() disabled=true selected=true>{cur2}</option>
                }.into_any())
            } else {
                None
            }
        }
    };
    let option_views: Vec<AnyView> = opts
        .iter()
        .map(|o| {
            let v = o.clone();
            let label = o.clone();
            let v_for_sel = o.clone();
            let current_b = current;
            view! {
                <option
                    value=v
                    prop:selected=move || value_to_display(&current_b()) == v_for_sel
                >{label}</option>
            }
            .into_any()
        })
        .collect();
    view! {
        <select
            class=FIELD_INPUT
            prop:value=move || value_to_display(&current())
            on:change=move |ev| {
                d(key, serde_json::Value::String(event_target_value(&ev)));
            }
        >
            {move || unknown_opt()}
            {option_views}
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
) -> AnyView {
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
) -> AnyView {
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

pub(super) fn value_to_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
