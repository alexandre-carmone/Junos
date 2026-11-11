//! Polar tools sheet, three sub-tabs. Drift: PHD2's Polar Drift Alignment
//! (`polardrift_toolwin.cpp`) over `pa_drift_open/start/stop/close` and
//! `guider_pa.drift` — the hemisphere and mirror are its profile keys,
//! read when the tool opens. Static (`staticpa_toolwin.cpp`) and Drift
//! align (`drift_tool.cpp`) send `pa_static_*` / `drift_align_*`; the
//! daemon answers "not available" until those tools land, shown as is.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{CARD, CARD_TITLE, CHECK, CHIP, LABEL, ROW};
use crate::i18n::{t, Lang};

use super::protocol::{call_error, Reply, Rpc};

const KEY_HEMISPHERE: &str = "/PolarDriftTool/Hemisphere";
const KEY_MIRROR: &str = "/PolarDriftTool/Mirror";

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Drift,
    Static,
    DriftAlign,
}

/// A number tile: label over value.
fn tile(label: impl Fn() -> &'static str + Send + Sync + 'static, value: impl Fn() -> String + Send + Sync + 'static) -> impl IntoView {
    view! {
        <div class="panel p-2 flex flex-col min-w-0">
            <span class="text-[11px] text-text-muted truncate">{move || label()}</span>
            <span class="font-mono text-base text-text-blue-bright">{move || value()}</span>
        </div>
    }
}

pub(super) fn pa_tools_sheet(pa: RwSignal<Value>, settings: RwSignal<Value>, rpc: Rpc, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    let rpc = StoredValue::new(rpc);
    let last = RwSignal::new(None::<Reply>);
    let call = move |method: &'static str, params: Value| last.set(Some(rpc.with_value(|r| r.call(method, params))));
    let tab = RwSignal::new(Tab::Drift);

    let drift = Memo::new(move |_| pa.with(|p| p["drift"].clone()));
    let open = move || drift.with(|d| !d.is_null());
    let drifting = move || drift.with(|d| d["drifting"].as_bool().unwrap_or(false));
    let num = move |k: &str| drift.with(|d| d["result"][k].as_f64());
    let fmt = move |k: &'static str, digits: usize, unit: &'static str| {
        move || num(k).map_or("\u{2014}".to_string(), |v| format!("{v:.digits$}{unit}"))
    };
    let key = move |k: &str| settings.with(|s| s[k].as_i64().unwrap_or(1));
    let north = move || key(KEY_HEMISPHERE) >= 0;
    let set_key = move |k: &'static str, v: i64| call("set_profile_value", json!({"key": k, "value": v}));

    let tab_chip = move |t: Tab, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button class=move || if tab.get() == t { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                on:click=move |_| tab.set(t)>{move || label(tr())}</button>
    };
    let hemi_chip = move |n: bool, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button class=move || if north() == n { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                disabled=open on:click=move |_| set_key(KEY_HEMISPHERE, if n { 1 } else { -1 })>
            {move || label(tr())}
        </button>
    };
    let button = move |label: fn(&'static crate::i18n::Translations) -> &'static str, class: &'static str, method: &'static str| view! {
        <button class=class on:click=move |_| call(method, json!({}))>{move || label(tr())}</button>
    };
    const GO: &str = "btn btn-primary h-11 px-5 font-semibold";
    const STOP: &str = "btn btn-danger h-11 px-5 font-semibold";
    const MORE: &str = "btn btn-ghost h-11 px-4";

    let drift_tab = move || view! {
        <div class=CARD>
            <div class="text-sm text-text whitespace-pre-line">{move || tr().guider_pa_drift_instructions}</div>
            <div class=ROW>
                <span class=format!("{LABEL} flex-1")>{move || tr().guider_pa_hemisphere}</span>
                {hemi_chip(true, |t| t.guider_pa_north)}
                {hemi_chip(false, |t| t.guider_pa_south)}
            </div>
            <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                <input type="checkbox" class=CHECK disabled=open prop:checked=move || key(KEY_MIRROR) < 0
                       on:change=move |ev| set_key(KEY_MIRROR, if crate::dom::event_target_checked(&ev) { -1 } else { 1 }) />
                <span class=LABEL>{move || tr().guider_pa_mirror}</span>
            </label>
            <Show when=open>
                <span class="text-xs text-text-muted">{move || tr().guider_pa_applies_on_open}</span>
            </Show>
            <div class="grid grid-cols-2 md:grid-cols-4 gap-2">
                {tile(move || tr().guider_pa_elapsed, fmt("time", 0, " s"))}
                {tile(move || tr().guider_pa_alpha, fmt("angle_deg", 1, "\u{00b0}"))}
                {tile(move || tr().guider_pa_offset, fmt("offset_px", 1, " px"))}
                {tile(move || tr().guider_pa_error, fmt("pa_error_arcmin", 1, "\u{2032}"))}
            </div>
            <div class="text-xs text-text-muted font-mono">
                {move || drift.with(|d| d["status"].as_array().map(|a| {
                    a.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{00b7} ")
                }).unwrap_or_default())}
            </div>
            <div class="flex gap-2 flex-wrap">
                <Show when=open fallback=move || button(|t| t.guider_pa_open, GO, "pa_drift_open")>
                    <Show when=drifting fallback=move || button(|t| t.guider_ga_start, GO, "pa_drift_start")>
                        {button(|t| t.guider_btn_stop, STOP, "pa_drift_stop")}
                    </Show>
                    {button(|t| t.guider_pa_close, MORE, "pa_drift_close")}
                </Show>
            </div>
        </div>
    };
    let static_tab = move || view! {
        <div class=CARD>
            <div class="text-sm text-text">{move || tr().guider_pa_static_hint}</div>
            <div class="flex gap-2 flex-wrap">
                {button(|t| t.guider_ga_start, GO, "pa_static_start")}
                {button(|t| t.guider_pa_rotate, MORE, "pa_static_rotate")}
                {button(|t| t.guider_pa_mark, MORE, "pa_static_mark")}
                {button(|t| t.guider_btn_stop, STOP, "pa_static_stop")}
            </div>
        </div>
    };
    let align_tab = move || view! {
        <div class=CARD>
            <div class="text-sm text-text whitespace-pre-line">{move || tr().guider_pa_drift_align_hint}</div>
            <div class="flex gap-2 flex-wrap">
                {button(|t| t.guider_pa_drift, GO, "drift_align_start")}
                {button(|t| t.guider_btn_stop, STOP, "drift_align_stop")}
            </div>
        </div>
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 pb-[max(1rem,env(safe-area-inset-bottom))] \
                    flex flex-col gap-3">
            <div class="flex gap-1.5 flex-wrap">
                <span class=format!("{CARD_TITLE} self-center mr-auto")>{move || tr().guider_pa_tool}</span>
                {tab_chip(Tab::Drift, |t| t.guider_pa_tab_drift)}
                {tab_chip(Tab::Static, |t| t.guider_pa_tab_static)}
                {tab_chip(Tab::DriftAlign, |t| t.guider_pa_tab_drift_align)}
            </div>
            {move || match tab.get() {
                Tab::Drift => drift_tab().into_any(),
                Tab::Static => static_tab().into_any(),
                Tab::DriftAlign => align_tab().into_any(),
            }}
            {move || call_error(last).map(|e| view! { <div class="text-sm text-state-err break-words">{e}</div> })}
        </div>
    }
}
