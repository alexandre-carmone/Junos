//! The one-shot capture form: exposure, frame type, filter, gain or ISO.
//!
//! Keys are KStars widget names (`kstars/ekos/capture/camera.ui`): what
//! `capture_get_all_settings` reports and `capture_set_all_settings` takes.

use std::collections::HashMap;

use gloo_timers::callback::Timeout;
use leptos::prelude::*;
use serde_json::{json, Value};

use crate::compat::{CameraSnapshot, CaptureSnapshot, FilterWheelSnapshot};
use crate::components::form::{setting_row, CHIP, NUM, SELECT};
use crate::components::frame_type::{frame_type_options, frame_type_pills};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::{dispatch_setting, send_device_property_set};

const SET_ALL: &str = "capture_set_all_settings";
const DEFAULT_GAIN: i64 = 100;
/// One-tap exposures in seconds, from focus frames (1 ms) to long subs.
const EXPOSURE_PRESETS: [f64; 8] = [0.001, 0.01, 0.1, 1.0, 5.0, 30.0, 60.0, 300.0];

/// A setting as text: numbers unquoted, nothing for Null.
pub(super) fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The capture settings as the form shows them: KStars' latest snapshot under
/// the user's edits. An edit stays pinned until KStars echoes it back, since
/// KStars may overwrite it meanwhile (a combo whose items aren't populated
/// yet, the gain "no value" sentinel…).
#[derive(Clone, Copy)]
pub(super) struct Settings {
    snapshot: Memo<Value>,
    pinned: RwSignal<HashMap<&'static str, Value>>,
    send: StoredValue<SendCmd>,
}

impl Settings {
    pub(super) fn new(capture: Signal<CaptureSnapshot>, send: SendCmd) -> Self {
        let s = Self {
            snapshot: Memo::new(move |_| capture.with(|c| c.settings.clone())),
            pinned: RwSignal::new(HashMap::new()),
            send: StoredValue::new(send),
        };
        // Unpin what KStars now reports.
        Effect::new(move |_| {
            let echoed: Vec<&'static str> = s.snapshot.with(|snap| {
                s.pinned.with_untracked(|m| {
                    m.iter().filter(|(k, v)| snap.get(**k) == Some(*v)).map(|(k, _)| *k).collect()
                })
            });
            if !echoed.is_empty() {
                s.pinned.update(|m| echoed.iter().for_each(|k| { m.remove(k); }));
            }
        });
        // The first snapshot without a real gain gets the default, so the
        // sequence jobs KStars builds from the form have one.
        let primed = StoredValue::new(false);
        Effect::new(move |_| {
            let missing = s.snapshot.with(|snap| {
                let obj = snap.as_object().filter(|o| !o.is_empty())?;
                Some(obj.get("captureGainN").and_then(Value::as_f64).is_none_or(|g| g < 0.0))
            });
            let Some(missing) = missing else { return };
            if !primed.get_value() {
                primed.set_value(true);
                if missing {
                    s.send.with_value(|send| dispatch_setting(send, SET_ALL, None, "captureGainN", DEFAULT_GAIN.into()));
                }
            }
        });
        s
    }

    /// The user's edit, else KStars' value, else a default. A negative gain is
    /// KStars' "no value" sentinel (`min - step`).
    pub(super) fn get(&self, key: &'static str) -> Value {
        if let Some(v) = self.pinned.with(|m| m.get(key).cloned()) {
            return v;
        }
        self.snapshot
            .with(|snap| snap.get(key).cloned())
            .filter(|v| !(key == "captureGainN" && v.as_f64().is_some_and(|g| g < 0.0)))
            .unwrap_or_else(|| match key {
                "captureExposureN" => Value::from(1.0),
                "captureTypeS" => Value::from("Light"),
                "captureGainN" => Value::from(DEFAULT_GAIN),
                _ => Value::Null,
            })
    }

    pub(super) fn str(&self, key: &'static str) -> String {
        text(&self.get(key))
    }

    /// Pin `value` and send it to KStars.
    fn set(&self, key: &'static str, value: Value) {
        self.pinned.update(|m| { m.insert(key, value.clone()); });
        self.send.with_value(|send| dispatch_setting(send, SET_ALL, None, key, value));
    }
}

/// Exposure, frame type, filter (with a wheel), then ISO for cameras that
/// report ISO steps (DSLRs, `CCD_ISO`), else gain.
pub(super) fn capture_form(
    s: Settings,
    camera: Signal<CameraSnapshot>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let filters = Memo::new(move |_| filter_wheel.with(|f| f.filter_names.clone()));
    let isos = Memo::new(move |_| camera.with(|c| c.iso_options.clone()));
    view! {
        {exposure_field(s, lang)}
        {frame_type_pills(
            move || frame_type_options(camera.with(|c| c.frame_type_options.clone())),
            move || s.str("captureTypeS"),
            move |v| s.set("captureTypeS", Value::String(v)),
        )}
        <Show when=move || filters.with(|f| !f.is_empty())>
            {setting_row(move || tr().field_filter, filter_select(s, filters, filter_wheel))}
        </Show>
        {move || if isos.with(Vec::is_empty) {
            setting_row(move || tr().field_gain, view! {
                <input type="number" min="0" step="1" inputmode="numeric" class=NUM
                       prop:value=move || s.str("captureGainN")
                       on:change=move |ev| {
                           if let Ok(g) = event_target_value(&ev).trim().parse::<i64>() {
                               s.set("captureGainN", g.max(0).into());
                           }
                       } />
            }).into_any()
        } else {
            setting_row(move || tr().field_iso, view! {
                <select class=SELECT on:change=move |ev| s.set("captureISOS", Value::String(event_target_value(&ev)))>
                    {options(s, "captureISOS", isos)}
                </select>
            }).into_any()
        }}
    }
}

/// The exposure input (KStars takes 0.001–3600 s, camera.ui:752) and preset
/// chips. Typing is sent 250 ms after the last key. The field follows KStars
/// only until first touched, so a value being typed is never overwritten.
fn exposure_field(s: Settings, lang: RwSignal<Lang>) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let touched = RwSignal::new(false);
    Effect::new(move |_| {
        let v = s.get("captureExposureN").as_f64().map(|n| n.to_string()).unwrap_or_default();
        if !touched.get_untracked() {
            text.set(v);
        }
    });
    // Timeout is !Send, hence local storage; replacing it cancels the send.
    let pending = StoredValue::new_local(None::<Timeout>);
    let send = move |secs: f64| {
        if secs.is_finite() && secs > 0.0 {
            s.set("captureExposureN", Value::from(secs));
        }
    };
    let on_input = move |ev| {
        let raw = event_target_value(&ev);
        touched.set(true);
        text.set(raw.clone());
        pending.set_value(Some(Timeout::new(250, move || {
            if let Ok(v) = raw.trim().parse() { send(v) }
        })));
    };
    let pick = move |secs: f64| {
        pending.set_value(None);
        touched.set(true);
        text.set(secs.to_string());
        send(secs);
    };
    let picked = move |secs: f64| text.with(|v| v.trim().parse::<f64>().is_ok_and(|v| (v - secs).abs() < 1e-6));

    view! {
        {setting_row(move || t(lang.get()).imaging_exposure, view! {
            <input type="number" min="0.001" max="3600" step="any" inputmode="decimal"
                   class="input input--sm font-mono text-base font-semibold w-[104px] shrink-0 text-right max-md:h-9"
                   prop:value=move || text.get() on:input=on_input />
            <span class="w-3 text-sm text-text-muted">"s"</span>
        })}
        <div class="grid grid-cols-4 gap-1.5">
            {EXPOSURE_PRESETS.map(|secs| view! {
                <button type="button"
                        class=move || if picked(secs) { format!("{CHIP} font-mono btn--active") } else { format!("{CHIP} font-mono") }
                        on:click=move |_| pick(secs)>
                    {preset_label(secs)}
                </button>
            }).to_vec()}
        </div>
    }
}

/// 0.001 → "1ms", 0.1 → "0.1s", 30 → "30s".
fn preset_label(secs: f64) -> String {
    if secs < 0.01 { format!("{}ms", (secs * 1000.0).round()) } else { format!("{secs}s") }
}

/// Picking a filter moves the wheel: `FilterPosCombo` alone only changes
/// KStars' combo (camera.cpp:245), so FILTER_SLOT (1-based) goes to the wheel.
fn filter_select(s: Settings, filters: Memo<Vec<String>>, filter_wheel: Signal<FilterWheelSnapshot>) -> impl IntoView {
    view! {
        <select class=SELECT on:change=move |ev| {
            let name = event_target_value(&ev);
            s.set("FilterPosCombo", Value::String(name.clone()));
            let wheel = filter_wheel.with_untracked(|f| f.device.clone());
            if let Some(i) = filters.with_untracked(|f| f.iter().position(|n| *n == name)).filter(|_| !wheel.is_empty()) {
                s.send.with_value(|send| send_device_property_set(send, &wheel, "FILTER_SLOT",
                    json!([{ "name": "FILTER_SLOT_VALUE", "value": i + 1 }])));
            }
        }>
            {options(s, "FilterPosCombo", filters)}
        </select>
    }
}

/// `<option>`s for `list`, the current value kept selectable when it isn't
/// offered.
fn options(s: Settings, key: &'static str, list: Memo<Vec<String>>) -> impl IntoView {
    move || {
        let cur = s.str(key);
        let mut opts = list.get();
        if !cur.is_empty() && !opts.contains(&cur) {
            opts.insert(0, cur.clone());
        }
        opts.into_iter()
            .map(|o| {
                let (selected, label) = (o == cur, o.clone());
                view! { <option value=o prop:selected=selected>{label}</option> }
            })
            .collect::<Vec<_>>()
    }
}
