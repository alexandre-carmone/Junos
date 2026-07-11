//! INDI device manager tab — web equivalent of KStars' INDI Control Panel.
//!
//! Left sidebar lists every device from `get_devices`; the panel shows all
//! INDI properties of the selected device grouped by INDI group, with
//! editable widgets per property type:
//!
//!   - numbers → slider (sane min/max/step) or input, buffered + SET
//!   - texts   → input, buffered + SET
//!   - switches → button group / select (1OFMANY, ATMOST1) or checkboxes
//!                (NOFMANY), applied immediately
//!   - lights  → read-only status LEDs
//!
//! Data flow: on device selection we `device_property_subscribe` with empty
//! `properties`/`groups` (= ALL properties, message.cpp:1727) and enumerate
//! via `device_get` (non-compact, message.cpp:1680). Updates arrive as
//! compact `device_property_get` pushes merged in `ws/store.rs`. We never
//! unsubscribe — per-name unsubscribes would clobber the module loops'
//! subscriptions on the same device.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::i18n::{Lang, t};
use crate::ws::{
    DeviceInfo, IndiElement, IndiElementValue, IndiProperty, IndiRule, IndiState, SendCmd,
};
use crate::ws_helpers::send_device_property_set;

const SECTION_CLS: &str = "fieldset m-0";
const LEGEND_CLS: &str = "fieldset__legend";
const INPUT_CLS: &str = "input input--sm flex-1 min-w-0 font-mono";
const ELEM_LABEL_CLS: &str =
    "basis-[clamp(90px,30%,180px)] grow-0 shrink-0 max-md:basis-full text-text-blue text-sm overflow-hidden text-ellipsis whitespace-nowrap";

fn state_color(s: IndiState) -> &'static str {
    match s {
        IndiState::Idle => "var(--text-muted)",
        IndiState::Ok => "var(--state-ok)",
        IndiState::Busy => "var(--state-warn)",
        IndiState::Alert => "var(--state-err)",
    }
}

fn state_led(color: &'static str) -> impl IntoView {
    view! {
        <span
            class="inline-block w-[9px] h-[9px] rounded-full shrink-0 border border-border-base"
            style=format!("background:{color}")
        ></span>
    }
}

/// Minimal INDI printf renderer. Handles the common `%<w>.<p>f` case;
/// sexagesimal (`%m`) and anything unrecognised fall back to a trimmed
/// plain rendering.
fn format_indi_number(format: &str, v: f64) -> String {
    if let Some(rest) = format.strip_prefix('%') {
        if let Some(f_pos) = rest.find('f') {
            let spec = &rest[..f_pos];
            let prec = spec
                .split_once('.')
                .and_then(|(_, p)| p.parse::<usize>().ok())
                .unwrap_or(2);
            return format!("{v:.prec$}");
        }
        if rest.ends_with('d') {
            return format!("{}", v.round() as i64);
        }
    }
    let s = format!("{v}");
    s
}

fn event_target_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

fn event_target_select_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlSelectElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

/// Subscribe (all properties) + enumerate one device, retrying until the
/// property list lands. KStars silently drops device commands while the
/// INDI driver isn't registered (message.cpp:1664) — same rationale as
/// `ws::retry::spawn_retry_property`.
fn spawn_device_fetch(
    send: SendCmd,
    device: String,
    props: RwSignal<HashMap<String, Vec<IndiProperty>>>,
) {
    use gloo_timers::future::TimeoutFuture;
    spawn_local(async move {
        let sub = serde_json::json!({
            "type": "device_property_subscribe",
            "payload": { "device": device, "properties": [], "groups": [] }
        })
        .to_string();
        let get = serde_json::json!({
            "type": "device_get",
            "payload": { "device": device, "compact": false }
        })
        .to_string();
        send(sub.clone());
        send(get.clone());
        for _ in 0..60 {
            TimeoutFuture::new(1_000).await;
            let ready = props.with_untracked(|m| {
                m.get(&device).map(|v| !v.is_empty()).unwrap_or(false)
            });
            if ready {
                return;
            }
            send(sub.clone());
            send(get.clone());
        }
        leptos::logging::log!("[devices] giving up enumerating {device} after 60s");
    });
}

#[component]
pub fn DevicesTab(
    devices: RwSignal<Vec<DeviceInfo>>,
    indi_properties: RwSignal<HashMap<String, Vec<IndiProperty>>>,
    indi_messages: RwSignal<HashMap<String, Vec<String>>>,
    online: RwSignal<bool>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let selected: RwSignal<Option<String>> = RwSignal::new(None);
    // Devices already subscribed+enumerated. Cleared when Ekos goes offline
    // so a profile restart re-fetches (subscriptions are lost server-side).
    let fetched: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());

    // Auto-select the first device; drop a selection that disappeared.
    Effect::new(move |_| {
        let devs = devices.get();
        let sel = selected.get_untracked();
        let still_there = sel
            .as_ref()
            .map(|s| devs.iter().any(|d| &d.name == s))
            .unwrap_or(false);
        if !still_there {
            selected.set(devs.first().map(|d| d.name.clone()));
        }
    });

    Effect::new(move |_| {
        if !online.get() {
            fetched.set(HashSet::new());
        }
    });

    // Lazy per-device fetch on selection (re-armed after reconnect since
    // `fetched` is cleared above and this effect tracks `online`).
    let send_fetch = Arc::clone(&send);
    Effect::new(move |_| {
        if !online.get() {
            return;
        }
        let Some(dev) = selected.get() else { return };
        if fetched.with_untracked(|f| f.contains(&dev)) {
            return;
        }
        fetched.update(|f| {
            f.insert(dev.clone());
        });
        spawn_device_fetch(Arc::clone(&send_fetch), dev, indi_properties);
    });

    // Properties of the selected device, grouped by INDI group in
    // definition order.
    let grouped = Signal::derive(move || {
        let Some(dev) = selected.get() else {
            return Vec::new();
        };
        indi_properties.with(|m| {
            let mut out: Vec<(String, Vec<IndiProperty>)> = Vec::new();
            for p in m.get(&dev).map(|v| v.as_slice()).unwrap_or(&[]) {
                match out.iter_mut().find(|(g, _)| *g == p.group) {
                    Some((_, v)) => v.push(p.clone()),
                    None => out.push((p.group.clone(), vec![p.clone()])),
                }
            }
            out
        })
    });

    let messages = Signal::derive(move || {
        let Some(dev) = selected.get() else {
            return Vec::new();
        };
        indi_messages.with(|m| {
            m.get(&dev)
                .map(|v| v.iter().rev().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });

    let send_rows = Arc::clone(&send);

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono grid grid-rows-[auto_1fr] overflow-hidden">

            // ── Header: device selector ───────────────────────────────
            <div class="flex items-center gap-x-sp-2 gap-y-sp-1 flex-wrap max-md:flex-nowrap max-md:overflow-x-auto md:flex-wrap min-h-[48px] py-sp-2 pr-5 max-md:pr-3 pl-20 border-b border-border-base bg-[rgba(6,6,15,0.85)]">
                <Show when=move || devices.with(|d| d.is_empty())>
                    <span class="text-sm text-text-muted">{move || tr().no_devices}</span>
                </Show>
                <For
                    each=move || devices.get()
                    key=|d| (d.name.clone(), d.connected)
                    children=move |d: DeviceInfo| {
                        let name = d.name.clone();
                        let name_click = d.name.clone();
                        let name_active = d.name.clone();
                        let dot = if d.connected { "var(--state-ok)" } else { "var(--text-muted)" };
                        view! {
                            <button
                                class="btn btn-ghost flex items-center gap-sp-1 text-sm shrink-0"
                                class:btn-primary=move || selected.get().as_deref() == Some(name_active.as_str())
                                on:click=move |_| selected.set(Some(name_click.clone()))
                            >
                                {state_led(dot)}
                                {name}
                            </button>
                        }
                    }
                />
            </div>

            // ── Body: grouped properties ──────────────────────────────
            <div class="overflow-y-auto py-4 px-5 max-md:px-3 flex flex-col gap-sp-4 max-w-[860px] w-full">

                <Show when=move || selected.with(|s| s.is_none())>
                    <div class="text-sm text-text-muted py-sp-3 px-sp-3 border border-border-base">
                        {move || tr().dev_select_device}
                    </div>
                </Show>

                <Show when=move || selected.with(|s| s.is_some()) && grouped.with(|g| g.is_empty())>
                    <div class="text-sm text-text-muted py-sp-3 px-sp-3 border border-border-base">
                        {move || if online.get() { tr().dev_loading_props } else { tr().disconnected }}
                    </div>
                </Show>

                <For
                    each=move || grouped.get()
                    key=move |(g, _)| format!("{:?}/{g}", selected.get_untracked())
                    children=move |(group, _props): (String, Vec<IndiProperty>)| {
                        let send_group = Arc::clone(&send_rows);
                        let group_key = group.clone();
                        view! {
                            <fieldset class=SECTION_CLS>
                                <legend class=LEGEND_CLS>{group.clone()}</legend>
                                <div class="flex flex-col">
                                    <For
                                        each={
                                            let group_key = group_key.clone();
                                            move || {
                                                grouped.with(|gs| {
                                                    gs.iter()
                                                        .find(|(g, _)| *g == group_key)
                                                        .map(|(_, ps)| ps.clone())
                                                        .unwrap_or_default()
                                                })
                                            }
                                        }
                                        // Structural key: rebuilds the row when the
                                        // compact placeholder is upgraded to the full
                                        // record or the element set changes; value
                                        // updates keep the DOM (and input focus).
                                        key=move |p: &IndiProperty| format!(
                                            "{:?}/{}/{}/{}",
                                            selected.get_untracked(), p.name, p.full, p.elements.len()
                                        )
                                        children=move |p: IndiProperty| {
                                            let device = selected.get_untracked().unwrap_or_default();
                                            view! {
                                                <PropertyRow
                                                    device=device
                                                    snapshot=p
                                                    indi_properties=indi_properties
                                                    send=Arc::clone(&send_group)
                                                />
                                            }
                                        }
                                    />
                                </div>
                            </fieldset>
                        }
                        .into_any()
                    }
                />

                // ── Device message log ────────────────────────────────
                <Show when=move || messages.with(|m| !m.is_empty())>
                    <details class="text-sm">
                        <summary class="cursor-pointer text-text-blue">
                            {move || tr().dev_messages_title}
                        </summary>
                        <div class="flex flex-col gap-[2px] pt-sp-2 text-text-muted">
                            {move || messages
                                .get()
                                .into_iter()
                                .map(|m| view! { <div>{m}</div> })
                                .collect::<Vec<_>>()}
                        </div>
                    </details>
                </Show>
            </div>
        </div>
    }
}

/// One INDI property: state LED + label + per-element widgets (+ SET for
/// buffered kinds). Structure comes from the `snapshot` the row was keyed
/// on; live values are read reactively from the store so pushed updates
/// refresh in place without rebuilding the DOM.
#[component]
fn PropertyRow(
    device: String,
    snapshot: IndiProperty,
    indi_properties: RwSignal<HashMap<String, Vec<IndiProperty>>>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let name = snapshot.name.clone();
    let writable = snapshot.perm.writable();
    let is_switch = snapshot.is_switch();
    let is_buffered = writable
        && matches!(
            snapshot.elements.first().map(|e| &e.value),
            Some(IndiElementValue::Number { .. }) | Some(IndiElementValue::Text(_))
        );

    // Live property record (values + state).
    let live = {
        let device = device.clone();
        let name = name.clone();
        Signal::derive(move || {
            indi_properties.with(|m| {
                m.get(&device)
                    .and_then(|v| v.iter().find(|p| p.name == name).cloned())
            })
        })
    };
    let prop_state = Signal::derive(move || live.with(|p| p.as_ref().map(|p| p.state).unwrap_or_default()));

    // Pending edits (element name → raw input string) for buffered kinds.
    let edits: RwSignal<HashMap<String, String>> = RwSignal::new(HashMap::new());

    // SET — send ALL elements (INDI vectors are atomic): pending edits where
    // present, current store values otherwise. Unparseable number input is
    // sent as a string — KStars runs f_scansexa on it (indistd.cpp:967).
    let on_set = {
        let send = Arc::clone(&send);
        let device = device.clone();
        let name = name.clone();
        move |_| {
            let Some(p) = live.get_untracked() else { return };
            let pending = edits.get_untracked();
            let els: Vec<serde_json::Value> = p
                .elements
                .iter()
                .filter_map(|e| match &e.value {
                    IndiElementValue::Number { value, .. } => {
                        Some(match pending.get(&e.name).map(|s| s.trim()) {
                            Some(s) if !s.is_empty() => match s.parse::<f64>() {
                                Ok(n) => serde_json::json!({ "name": e.name, "value": n }),
                                Err(_) => serde_json::json!({ "name": e.name, "value": s }),
                            },
                            _ => serde_json::json!({ "name": e.name, "value": value }),
                        })
                    }
                    IndiElementValue::Text(t) => {
                        let v = pending.get(&e.name).cloned().unwrap_or_else(|| t.clone());
                        Some(serde_json::json!({ "name": e.name, "text": v }))
                    }
                    _ => None,
                })
                .collect();
            if !els.is_empty() {
                send_device_property_set(&send, &device, &name, serde_json::Value::Array(els));
            }
            edits.set(HashMap::new());
        }
    };

    let elements_view: Vec<AnyView> = if is_switch {
        vec![render_switch_property(
            &device,
            &snapshot,
            live,
            Arc::clone(&send),
        )]
    } else {
        snapshot
            .elements
            .iter()
            .map(|e| render_scalar_element(e, writable, live, edits))
            .collect()
    };

    let title = snapshot.name.clone();
    let label = snapshot.label.clone();
    view! {
        <div class="border-b border-border-base py-sp-2 flex flex-col gap-sp-1 last:border-b-0">
            <div class="flex items-center gap-sp-2">
                <span
                    class="inline-block w-[9px] h-[9px] rounded-full shrink-0 border border-border-base"
                    style=move || format!("background:{}", state_color(prop_state.get()))
                ></span>
                <span class="text-sm overflow-hidden text-ellipsis whitespace-nowrap" title=title>
                    {label}
                </span>
                <Show when=move || is_buffered>
                    <button
                        class="btn btn-primary ml-auto text-xs py-[2px] px-sp-2"
                        disabled=move || prop_state.get() == IndiState::Busy
                        on:click=on_set.clone()
                    >
                        {move || t(lang.get()).set_btn}
                    </button>
                </Show>
            </div>
            <div class="flex flex-col gap-sp-1 pl-[17px]">
                {elements_view}
            </div>
        </div>
    }
}

/// Live lookup of one element's value inside the property record.
fn element_value(
    live: Signal<Option<IndiProperty>>,
    el_name: &str,
) -> impl Fn() -> Option<IndiElementValue> + Clone + Send + Sync + 'static {
    let el_name = el_name.to_string();
    move || {
        live.with(|p| {
            p.as_ref()
                .and_then(|p| p.elements.iter().find(|e| e.name == el_name))
                .map(|e| e.value.clone())
        })
    }
}

/// Number / text / light element row: label + widget.
fn render_scalar_element(
    el: &IndiElement,
    writable: bool,
    live: Signal<Option<IndiProperty>>,
    edits: RwSignal<HashMap<String, String>>,
) -> AnyView {
    let el_name = el.name.clone();
    let label = el.label.clone();
    let value = element_value(live, &el_name);

    let widget: AnyView = match &el.value {
        IndiElementValue::Number { min, max, step, format, .. } => {
            let format = format.clone();
            let fmt2 = format.clone();
            let current = {
                let value = value.clone();
                move || match value() {
                    Some(IndiElementValue::Number { value, .. }) => value,
                    _ => 0.0,
                }
            };
            let current_txt = {
                let current = current.clone();
                move || format_indi_number(&fmt2, current())
            };
            if !writable {
                view! { <span class="text-sm">{current_txt}</span> }.into_any()
            } else {
                // Display: pending edit wins over the live value so pushes
                // don't stomp typing; SET or a row rebuild clears the buffer.
                let name_edit = el_name.clone();
                let name_input = el_name.clone();
                let display = {
                    let current_txt = current_txt.clone();
                    move || {
                        edits.with(|e| e.get(&name_edit).cloned())
                            .unwrap_or_else(|| current_txt())
                    }
                };
                let on_input = move |ev: web_sys::Event| {
                    let v = event_target_value(&ev);
                    edits.update(|e| {
                        e.insert(name_input.clone(), v);
                    });
                };
                let sane_slider = *min < *max
                    && min.is_finite()
                    && max.is_finite()
                    && *step > 0.0
                    && (*max - *min) / *step <= 1000.0
                    && !format.contains('m'); // sexagesimal — no slider
                if sane_slider {
                    let name_slider = el_name.clone();
                    let slider_val = {
                        let value = value.clone();
                        move || {
                            edits.with(|e| e.get(&name_slider).and_then(|s| s.parse::<f64>().ok()))
                                .unwrap_or_else(|| match value() {
                                    Some(IndiElementValue::Number { value, .. }) => value,
                                    _ => 0.0,
                                })
                                .to_string()
                        }
                    };
                    view! {
                        <input
                            type="range"
                            class="flex-1 min-w-[80px]"
                            min=min.to_string()
                            max=max.to_string()
                            step=step.to_string()
                            prop:value=slider_val
                            on:input=on_input
                        />
                        <span class="text-sm w-[72px] text-right shrink-0 max-md:w-auto max-md:text-left">{display}</span>
                    }
                    .into_any()
                } else {
                    // Plain text input so sexagesimal strings ("12:30:00")
                    // stay typeable — KStars parses them server-side.
                    view! {
                        <span class="text-sm text-text-muted w-[88px] text-right shrink-0 max-md:w-auto max-md:text-left overflow-hidden text-ellipsis">
                            {current_txt}
                        </span>
                        <input
                            type="text"
                            inputmode="decimal"
                            class=INPUT_CLS
                            prop:value=display
                            on:input=on_input
                        />
                    }
                    .into_any()
                }
            }
        }
        IndiElementValue::Text(_) => {
            let current = {
                let value = value.clone();
                move || match value() {
                    Some(IndiElementValue::Text(t)) => t,
                    _ => String::new(),
                }
            };
            if !writable {
                view! { <span class="text-sm break-all">{current}</span> }.into_any()
            } else {
                let name_edit = el_name.clone();
                let name_input = el_name.clone();
                let display = {
                    let current = current.clone();
                    move || {
                        edits.with(|e| e.get(&name_edit).cloned())
                            .unwrap_or_else(|| current())
                    }
                };
                view! {
                    <input
                        type="text"
                        class=INPUT_CLS
                        prop:value=display
                        on:input=move |ev| {
                            let v = event_target_value(&ev);
                            edits.update(|e| {
                                e.insert(name_input.clone(), v);
                            });
                        }
                    />
                }
                .into_any()
            }
        }
        IndiElementValue::Light(_) => {
            let color = {
                let value = value.clone();
                move || match value() {
                    Some(IndiElementValue::Light(s)) => state_color(s),
                    _ => "var(--text-muted)",
                }
            };
            view! {
                <span
                    class="inline-block w-[9px] h-[9px] rounded-full shrink-0 border border-border-base"
                    style=move || format!("background:{}", color())
                ></span>
            }
            .into_any()
        }
        // Switches are rendered whole-property in render_switch_property.
        IndiElementValue::Switch(_) => view! { <span></span> }.into_any(),
    };

    view! {
        <div class="flex items-center gap-sp-2 text-sm min-h-[26px] max-md:flex-wrap">
            <span class=ELEM_LABEL_CLS title=el_name>{label}</span>
            {widget}
        </div>
    }
    .into_any()
}

/// Whole switch property as one control row. 1OFMANY/ATMOST1 → button group
/// (≤6 options) or <select>; NOFMANY → checkboxes; read-only → static dots.
/// Writes apply immediately: KStars resets exclusive vectors before applying
/// (indistd.cpp:941), so sending just the target element is enough.
fn render_switch_property(
    device: &str,
    snapshot: &IndiProperty,
    live: Signal<Option<IndiProperty>>,
    send: SendCmd,
) -> AnyView {
    let writable = snapshot.perm.writable();
    let exclusive = matches!(snapshot.rule, IndiRule::OneOfMany | IndiRule::AtMostOne);
    let at_most_one = snapshot.rule == IndiRule::AtMostOne;
    let device = device.to_string();
    let prop_name = snapshot.name.clone();

    let el_on = move |live: Signal<Option<IndiProperty>>, el: &str| -> bool {
        let el = el.to_string();
        live.with(|p| {
            p.as_ref()
                .and_then(|p| p.elements.iter().find(|e| e.name == el))
                .map(|e| matches!(e.value, IndiElementValue::Switch(true)))
                .unwrap_or(false)
        })
    };

    if !writable {
        let items: Vec<AnyView> = snapshot
            .elements
            .iter()
            .map(|e| {
                let el_name = e.name.clone();
                let label = e.label.clone();
                let on = {
                    let el_on = el_on.clone();
                    move || el_on(live, &el_name)
                };
                view! {
                    <span class="flex items-center gap-sp-1 text-sm">
                        <span
                            class="inline-block w-[9px] h-[9px] rounded-full border border-border-base"
                            style=move || format!(
                                "background:{}",
                                if on() { "var(--state-ok)" } else { "var(--text-muted)" }
                            )
                        ></span>
                        {label}
                    </span>
                }
                .into_any()
            })
            .collect();
        return view! { <div class="flex items-center gap-sp-3 flex-wrap">{items}</div> }
            .into_any();
    }

    if exclusive && snapshot.elements.len() > 6 {
        // Dropdown of element labels; change sends the ON element only.
        let options: Vec<(String, String)> = snapshot
            .elements
            .iter()
            .map(|e| (e.name.clone(), e.label.clone()))
            .collect();
        let active = {
            let names: Vec<String> = options.iter().map(|(n, _)| n.clone()).collect();
            let el_on = el_on.clone();
            move || {
                names
                    .iter()
                    .find(|n| el_on(live, n))
                    .cloned()
                    .unwrap_or_default()
            }
        };
        let on_change = move |ev: web_sys::Event| {
            let sel = event_target_select_value(&ev);
            if !sel.is_empty() {
                send_device_property_set(
                    &send,
                    &device,
                    &prop_name,
                    serde_json::json!([{ "name": sel, "state": 1 }]),
                );
            }
        };
        return view! {
            <select class=INPUT_CLS prop:value=active on:change=on_change>
                {options
                    .into_iter()
                    .map(|(n, l)| view! { <option value=n>{l}</option> })
                    .collect::<Vec<_>>()}
            </select>
        }
        .into_any();
    }

    // Button group (exclusive) / checkboxes (NOFMANY).
    let items: Vec<AnyView> = snapshot
        .elements
        .iter()
        .map(|e| {
            let el_name = e.name.clone();
            let label = e.label.clone();
            let on = {
                let el_on = el_on.clone();
                let el_name = el_name.clone();
                move || el_on(live, &el_name)
            };
            let send = Arc::clone(&send);
            let device = device.clone();
            let prop_name = prop_name.clone();
            if exclusive {
                let on_click = {
                    let on = on.clone();
                    move |_| {
                        // ATMOST1 allows all-off: re-clicking the active
                        // element turns it off.
                        let new_state = if at_most_one && on() { 0 } else { 1 };
                        send_device_property_set(
                            &send,
                            &device,
                            &prop_name,
                            serde_json::json!([{ "name": el_name, "state": new_state }]),
                        );
                    }
                };
                view! {
                    <button
                        class="btn btn-ghost text-sm py-[2px]"
                        class:btn-primary=on.clone()
                        on:click=on_click
                    >
                        {label}
                    </button>
                }
                .into_any()
            } else {
                let on_change = move |ev: web_sys::Event| {
                    let checked = ev
                        .target()
                        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                        .map(|el| el.checked())
                        .unwrap_or(false);
                    send_device_property_set(
                        &send,
                        &device,
                        &prop_name,
                        serde_json::json!([{ "name": el_name, "state": if checked { 1 } else { 0 } }]),
                    );
                };
                view! {
                    <label class="flex items-center gap-sp-1 text-sm cursor-pointer">
                        <input type="checkbox" prop:checked=on.clone() on:change=on_change />
                        {label}
                    </label>
                }
                .into_any()
            }
        })
        .collect();

    view! { <div class="flex items-center gap-sp-2 flex-wrap">{items}</div> }.into_any()
}
