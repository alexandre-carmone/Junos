//! The Calibration sheet's assistant card: PHD2's Calibration Assistant
//! (`calibration_assistant.cpp`) over `guider_tools.cal_assistant`. The
//! current pointing, the recommended place to calibrate (meridian offset,
//! Dec, side; `cal_assistant_set`), its custom-location buttons
//! (`cal_assistant_location {action, single_side}`), Slew, Calibrate and
//! Cancel, the warnings and the last result with PHD2's verdict.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{CARD, CARD_TITLE, CHECK, CHIP, LABEL, NUM, ROW};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang};

use super::protocol::{call_error, Reply, Rpc};

const MORE: &str = "btn btn-ghost h-11 px-4";

pub(super) fn cal_assistant_card(tools: RwSignal<Value>, rpc: Rpc, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    let rpc = StoredValue::new(rpc);
    let last = RwSignal::new(None::<Reply>);
    let call = move |method: &'static str, params: Value| last.set(Some(rpc.with_value(|r| r.call(method, params))));

    let ca = Memo::new(move |_| tools.with(|t| t["cal_assistant"].clone()));
    let open = move || ca.with(|c| !c.is_null());
    let flag = move |k: &'static str| move || ca.with(|c| c[k].as_bool().unwrap_or(false));
    let text = move |k: &'static str| move || ca.with(|c| c[k].as_str().unwrap_or("").to_string());
    let busy = move || flag("slewing")() || flag("calibrating")();

    // The target as typed, refreshed from the window's.
    let offset = RwSignal::new(String::new());
    let dec = RwSignal::new(String::new());
    let target = Memo::new(move |_| ca.with(|c| c["target"].clone()));
    Effect::new(move |_| {
        let tgt = target.get();
        offset.set(tgt["offset"].as_i64().map(|v| v.to_string()).unwrap_or_default());
        dec.set(tgt["dec"].as_i64().map(|v| v.to_string()).unwrap_or_default());
    });
    let set = move |east: Option<bool>| {
        let int = |s: RwSignal<String>| s.with_untracked(|s| s.trim().parse::<i64>().ok());
        call("cal_assistant_set", json!({"offset": int(offset), "dec": int(dec), "east": east}));
    };
    let east = move || ca.with(|c| c["target"]["east"].as_bool().unwrap_or(false));
    let side = move |e: bool, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button class=move || if east() == e { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                disabled=busy on:click=move |_| set(Some(e))>{move || label(tr())}</button>
    };
    let field = move |label: fn(&'static crate::i18n::Translations) -> &'static str, v: RwSignal<String>| view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label(tr())}</span>
            <input type="number" inputmode="numeric" step="1" class=NUM disabled=busy
                   prop:value=move || v.get()
                   on:input=move |ev| v.set(event_target_value(&ev))
                   on:change=move |_| set(None) />
        </div>
    };
    let single_side = RwSignal::new(false);
    let location = move |action: &'static str, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button class=CHIP disabled=busy
                on:click=move |_| call("cal_assistant_location", json!({"action": action, "single_side": single_side.get_untracked()}))>
            {move || label(tr())}
        </button>
    };
    let current = move || {
        ca.with(|c| {
            let cur = &c["current"];
            let side = if cur["east"].as_bool().unwrap_or(false) { tr().pa_east } else { tr().pa_west };
            format!("{}\u{00b0} \u{00b7} {} {}\u{00b0} \u{00b7} {side}",
                    cur["offset"].as_str().unwrap_or("?"), tr().dec_label, cur["dec"].as_str().unwrap_or("?"))
        })
    };

    view! {
        <div class=CARD>
            <span class=CARD_TITLE>{move || tr().guider_ca_title}</span>
            <Show when=open fallback=move || view! {
                <span class="text-xs text-text-muted">{move || tr().guider_ca_hint}</span>
                <div>
                    <button class="btn btn-primary h-11 px-5 font-semibold" on:click=move |_| call("cal_assistant_start", json!({}))>
                        {move || tr().guider_pa_open}
                    </button>
                </div>
            }>
                <div class=ROW>
                    <span class=format!("{LABEL} flex-1")>{move || tr().guider_ca_current}</span>
                    <span class="font-mono text-sm text-text-dim">{current}</span>
                </div>
                <span class="text-xs text-text-muted">{move || tr().guider_ca_target}</span>
                {field(|t| t.guider_ca_offset, offset)}
                {field(|t| t.guider_cal_declination, dec)}
                <div class="flex gap-1.5 flex-wrap">
                    {side(true, |t| t.pa_east)}
                    {side(false, |t| t.pa_west)}
                </div>
                <div class="flex gap-1.5 flex-wrap items-center">
                    {location("restore", |t| t.guider_ca_recommended)}
                    {location("load", |t| t.guider_ca_load)}
                    {location("save", |t| t.guider_ca_save)}
                    <label class="flex items-center gap-2 min-h-9 cursor-pointer">
                        <input type="checkbox" class=CHECK prop:checked=move || single_side.get()
                               on:change=move |ev| single_side.set(event_target_checked(&ev)) />
                        <span class="text-xs text-text-muted">{move || tr().guider_ca_single_side}</span>
                    </label>
                </div>
                <div class=move || if flag("fatal")() { "text-sm text-state-err break-words" } else { "text-sm text-state-warn break-words" }>
                    {text("warning")}
                </div>
                <div class="text-xs text-text-muted whitespace-pre-line">{text("explanation")}</div>
                <div class="text-sm text-text-blue break-words">{text("status")}</div>
                <Show when=move || ca.with(|c| c["verdict"].is_string() || c["sanity"].is_string())>
                    <div class="text-sm break-words">
                        <span class="font-semibold text-text-blue-bright">{text("verdict")}</span>
                        " " <span class="text-text-dim whitespace-pre-line">{text("last_result")}</span>
                    </div>
                    <div class="text-xs text-state-warn break-words">{text("sanity")}</div>
                </Show>
                <div class="flex gap-2 flex-wrap">
                    <button class=MORE disabled=move || busy() || flag("fatal")() || !flag("can_slew")()
                            on:click=move |_| call("cal_assistant_slew", json!({}))>{move || tr().guider_slew}</button>
                    <Show when=busy fallback=move || view! {
                        <button class="btn btn-primary h-11 px-5 font-semibold" disabled=flag("fatal")
                                on:click=move |_| call("cal_assistant_calibrate", json!({}))>
                            {move || tr().guider_ca_calibrate}
                        </button>
                    }>
                        <button class="btn btn-danger h-11 px-5 font-semibold"
                                on:click=move |_| call("cal_assistant_cancel", json!({}))>{move || tr().cancel}</button>
                    </Show>
                    <button class=MORE on:click=move |_| call("cal_assistant_stop", json!({}))>{move || tr().guider_pa_close}</button>
                </div>
            </Show>
            {move || call_error(last).map(|e| view! { <div class="text-sm text-state-err break-words">{e}</div> })}
        </div>
    }
}
