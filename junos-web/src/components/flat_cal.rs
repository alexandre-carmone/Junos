//! Flat-panel / dust-cap calibration tab.
//!
//! Wire protocol: KStars `commands.h:367-369` declares `cap_park`,
//! `cap_unpark`, `cap_set_light` but `message.cpp` has no handlers as of
//! the current snapshot — sending them is a no-op. So this tab drives the
//! underlying INDI properties directly via `device_property_set`:
//!
//!   - **Park / Unpark** → `CAP_PARK` switch with elements `PARK` / `UNPARK`
//!     (kstars/indi/indidustcap.cpp:34, 115-159).
//!   - **Light on / off** → `FLAT_LIGHT_CONTROL` switch, element
//!     `FLAT_LIGHT_ON` / `FLAT_LIGHT_OFF` (indilightbox.cpp:36, 47).
//!   - **Brightness**    → `FLAT_LIGHT_INTENSITY` number, element
//!     `FLAT_LIGHT_INTENSITY_VALUE` (indilightbox.cpp:63, 114).
//!
//! Inbound state is fed from `compat::DustCapSnapshot`, which is populated
//! from the `get_devices` device-list scan (interface bits 1<<9 dustcap,
//! 1<<10 lightbox) plus pushed INDI property updates handled in
//! `ws/store.rs`.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::compat::DustCapSnapshot;
use crate::i18n::{Lang, t};
use crate::ws::{DustCapParkState, SendCmd};
use crate::ws_helpers::send_device_property_set;

const SECTION_CLS: &str = "fieldset m-0";
const LEGEND_CLS:  &str = "fieldset__legend";
const ROW_CLS:     &str = "flex items-center gap-sp-3 flex-wrap";
const LABEL_CLS:   &str = "basis-[clamp(100px,25%,180px)] grow-0 shrink-0 text-text-blue text-sm";
const INPUT_CLS:   &str = "input input--sm flex-1 min-w-0 font-mono";

fn park_state_color(s: DustCapParkState) -> &'static str {
    match s {
        DustCapParkState::Parked   => "var(--state-info)",
        DustCapParkState::Unparked => "var(--state-ok)",
        DustCapParkState::Moving   => "var(--state-warn)",
        DustCapParkState::Unknown  => "var(--text-muted)",
    }
}

fn park_state_label(s: DustCapParkState, tr: &crate::i18n::Translations) -> &'static str {
    match s {
        DustCapParkState::Parked   => tr.fc_status_parked,
        DustCapParkState::Unparked => tr.fc_status_unparked,
        DustCapParkState::Moving   => tr.fc_status_moving,
        DustCapParkState::Unknown  => tr.fc_status_unknown,
    }
}

fn event_target_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

fn set_cap_park(send: &SendCmd, device: &str, park: bool) {
    send_device_property_set(send, device, "CAP_PARK", serde_json::json!({
        "switches": [
            { "name": "PARK",   "value": park },
            { "name": "UNPARK", "value": !park },
        ]
    }));
}

fn set_light(send: &SendCmd, device: &str, on: bool) {
    send_device_property_set(send, device, "FLAT_LIGHT_CONTROL", serde_json::json!({
        "switches": [
            { "name": "FLAT_LIGHT_ON",  "value": on },
            { "name": "FLAT_LIGHT_OFF", "value": !on },
        ]
    }));
}

fn set_brightness(send: &SendCmd, device: &str, value: f64) {
    send_device_property_set(send, device, "FLAT_LIGHT_INTENSITY", serde_json::json!({
        "numbers": [
            // The element name varies across drivers — `FLAT_LIGHT_INTENSITY_VALUE`
            // is the canonical INDI name, but some drivers expose `FLAT_INTENSITY`.
            // Sending the canonical name; FLAT_INTENSITY drivers should still
            // accept it via INDI's name-lookup fallback.
            { "name": "FLAT_LIGHT_INTENSITY_VALUE", "value": value }
        ]
    }));
}

#[component]
pub fn FlatCalTab(
    #[prop(into)] cap: Signal<DustCapSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Local brightness draft so the slider tracks the finger smoothly; we
    // dispatch on `change` (release) rather than `input` (every tick) to
    // avoid flooding the WS with INDI numbers — a debounce-by-event approach.
    let brightness_draft = RwSignal::new(0.0_f64);
    // Seed once when the server-side value first arrives, and refresh when
    // it changes externally.
    Effect::new(move |_| {
        if let Some(v) = cap.with(|c| c.brightness) {
            brightness_draft.set(v);
        }
    });

    let has_device = move || cap.with(|c| !c.device.is_empty());

    let send_park = send.clone();
    let on_park = move |_| {
        let dev = cap.with(|c| c.device.clone());
        if !dev.is_empty() { set_cap_park(&send_park, &dev, true); }
    };
    let send_unpark = send.clone();
    let on_unpark = move |_| {
        let dev = cap.with(|c| c.device.clone());
        if !dev.is_empty() { set_cap_park(&send_unpark, &dev, false); }
    };

    let send_light_on = send.clone();
    let on_light_on = move |_| {
        let dev = cap.with(|c| c.device.clone());
        if !dev.is_empty() { set_light(&send_light_on, &dev, true); }
    };
    let send_light_off = send.clone();
    let on_light_off = move |_| {
        let dev = cap.with(|c| c.device.clone());
        if !dev.is_empty() { set_light(&send_light_off, &dev, false); }
    };

    let send_brightness = send.clone();
    let on_brightness_change = move |ev: web_sys::Event| {
        let Ok(v) = event_target_value(&ev).parse::<f64>() else { return };
        brightness_draft.set(v);
        let dev = cap.with(|c| c.device.clone());
        if !dev.is_empty() { set_brightness(&send_brightness, &dev, v); }
    };
    let on_brightness_input = move |ev: web_sys::Event| {
        // Track-only update on every tick so the slider feels responsive;
        // network dispatch waits for `change` (above).
        if let Ok(v) = event_target_value(&ev).parse::<f64>() { brightness_draft.set(v); }
    };

    let park_state = move || cap.with(|c| c.park_state);
    let park_disabled    = move || matches!(park_state(), DustCapParkState::Parked   | DustCapParkState::Moving);
    let unpark_disabled  = move || matches!(park_state(), DustCapParkState::Unparked | DustCapParkState::Moving);

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono grid grid-rows-[auto_1fr] overflow-hidden">

            // ── Header ────────────────────────────────────────────────
            <div class="flex items-center gap-y-sp-2 gap-x-sp-4 flex-wrap min-h-[48px] py-sp-2 pr-5 pl-20 border-b border-border-base bg-[rgba(6,6,15,0.85)] text-md">
                <span class="inline-block py-1 px-sp-3 rounded-[14px] text-sm border border-current"
                      style=move || format!("color:{}", park_state_color(park_state()))>
                    {move || park_state_label(park_state(), t(lang.get()))}
                </span>
                <span class="text-text-blue">{move || tr().dc_device}</span>
                <span>{move || {
                    let d = cap.with(|c| c.device.clone());
                    if d.is_empty() { "—".to_string() } else { d }
                }}</span>
            </div>

            // ── Body ──────────────────────────────────────────────────
            <div class="overflow-y-auto py-4 px-5 flex flex-col gap-sp-4 max-w-[640px]">

                // Empty-state when no cap device is on the active profile.
                <Show when=move || !has_device()>
                    <div class="text-sm text-text-muted py-sp-3 px-sp-3 border border-border-base">
                        {move || tr().fc_no_device}
                    </div>
                </Show>

                // ── Dust cap section ──────────────────────────────────
                <fieldset class=SECTION_CLS>
                    <legend class=LEGEND_CLS>{move || tr().fc_section_dust_cap}</legend>
                    <div class="flex flex-col gap-sp-3">
                        <div class=ROW_CLS>
                            <span class=LABEL_CLS>{move || tr().fc_state}</span>
                            <span class="text-sm" style=move || format!("color:{}", park_state_color(park_state()))>
                                {move || park_state_label(park_state(), t(lang.get()))}
                            </span>
                        </div>
                        <div class="flex flex-wrap gap-sp-2">
                            <button
                                class="btn btn-primary"
                                on:click=on_park.clone()
                                disabled=move || !has_device() || park_disabled()>
                                {move || tr().dc_park}
                            </button>
                            <button
                                class="btn btn-primary"
                                on:click=on_unpark.clone()
                                disabled=move || !has_device() || unpark_disabled()>
                                {move || tr().dc_unpark}
                            </button>
                        </div>
                    </div>
                </fieldset>

                // ── Flat panel section ────────────────────────────────
                <fieldset class=SECTION_CLS>
                    <legend class=LEGEND_CLS>{move || tr().fc_light_panel}</legend>
                    <Show
                        when=move || cap.with(|c| c.has_light_panel)
                        fallback=move || view! {
                            <div class="text-sm text-text-muted">{move || tr().fc_no_device}</div>
                        }
                    >
                        <div class="flex flex-col gap-sp-3">
                            <div class="flex flex-wrap gap-sp-2">
                                <button
                                    class="btn btn-primary"
                                    on:click=on_light_on.clone()
                                    disabled=move || !has_device() || cap.with(|c| c.light_on == Some(true))>
                                    {move || tr().fc_light_on}
                                </button>
                                <button
                                    class="btn btn-ghost"
                                    on:click=on_light_off.clone()
                                    disabled=move || !has_device() || cap.with(|c| c.light_on == Some(false))>
                                    {move || tr().fc_light_off}
                                </button>
                            </div>
                            <div class=ROW_CLS>
                                <span class=LABEL_CLS>{move || tr().fc_brightness_label}</span>
                                <input
                                    type="range"
                                    class="flex-1 min-w-[160px]"
                                    min=move || cap.with(|c| c.brightness_min.unwrap_or(0.0)).to_string()
                                    max=move || cap.with(|c| c.brightness_max.unwrap_or(255.0)).to_string()
                                    step="1"
                                    on:input=on_brightness_input.clone()
                                    on:change=on_brightness_change.clone()
                                    prop:value=move || brightness_draft.get().to_string()
                                    disabled=move || !has_device()
                                />
                                <input
                                    type="number"
                                    class=INPUT_CLS
                                    min=move || cap.with(|c| c.brightness_min.unwrap_or(0.0)).to_string()
                                    max=move || cap.with(|c| c.brightness_max.unwrap_or(255.0)).to_string()
                                    step="1"
                                    on:change=on_brightness_change.clone()
                                    prop:value=move || format!("{:.0}", brightness_draft.get())
                                    disabled=move || !has_device()
                                />
                            </div>
                        </div>
                    </Show>
                </fieldset>
            </div>
        </div>
    }
}
