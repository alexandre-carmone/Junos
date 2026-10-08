//! INDI device manager tab — web equivalent of KStars' INDI Control Panel.
//!
//! Layout (phone-first, like Guide / Scheduler): a header (the selected
//! device's connection badge), the device list — a chip strip on phones, a
//! sidebar from `md` — then pills for the device's INDI groups, one card per
//! property of the chosen group, and a pinned footer with the latest device
//! message (tap → all of them in a sheet). Widgets per property type:
//!
//!   - numbers / texts → input, buffered; Set (or Enter) sends the vector
//!   - switches → pills / select (1OFMANY, ATMOST1) or checkboxes
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
use wasm_bindgen_futures::spawn_local;

use crate::components::form::{sheet, CARD, CHECK, CHIP, FOOTER, LABEL, ROW};
use crate::components::tab_wheel_icons::tab_icon;
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{Lang, t};
use crate::ws::{
    DeviceInfo, IndiElement, IndiElementValue, IndiProperty, IndiRule, IndiState, SendCmd,
};
use crate::ws_helpers::send_device_property_set;
use crate::Tab;

const LED: &str = "inline-block w-2.5 h-2.5 rounded-full shrink-0";
const INPUT: &str = "input input--sm font-mono min-w-0 max-md:h-9";
/// Device pill on phones, full-width sidebar row from md.
const DEVICE: &str = "chip shrink-0 max-w-[14rem] h-9 gap-2 px-3 cursor-pointer \
                      md:max-w-none md:w-full md:h-10 md:rounded-md";

fn led(s: IndiState) -> &'static str {
    match s {
        IndiState::Idle => "bg-text-muted",
        IndiState::Ok => "bg-state-ok",
        IndiState::Busy => "bg-state-warn animate-pulse",
        IndiState::Alert => "bg-state-err",
    }
}

/// Kind icon from the libindi driver-interface bits (basedevice.h).
fn device_icon(iface: i64) -> &'static str {
    let tab = match iface {
        i if i & 1 != 0 => Tab::Mount,                         // TELESCOPE
        i if i & (1 << 1) != 0 => Tab::Imaging,                // CCD
        i if i & (1 << 2) != 0 => Tab::Guide,                  // GUIDER
        i if i & (1 << 3) != 0 => Tab::Focus,                  // FOCUSER
        _ => Tab::Devices,
    };
    tab_icon(tab)
}

/// The live `CONNECTION` switch once the device is mirrored, else the
/// `get_devices` flag.
fn is_connected(props: &HashMap<String, Vec<IndiProperty>>, d: &DeviceInfo) -> bool {
    props
        .get(&d.name)
        .and_then(|ps| ps.iter().find(|p| p.name == "CONNECTION"))
        .and_then(|p| p.elements.iter().find(|e| e.name == "CONNECT"))
        .map_or(d.connected, |e| matches!(e.value, IndiElementValue::Switch(true)))
}

/// Minimal INDI printf renderer: `%<w>.<p>f`, `%d` and sexagesimal
/// `%<w>.<f>m`; anything else falls back to a plain rendering.
fn format_indi_number(format: &str, v: f64) -> String {
    let Some(rest) = format.strip_prefix('%') else { return v.to_string() };
    let prec = |spec: &str| spec.split_once('.').and_then(|(_, p)| p.parse::<usize>().ok());
    if let Some(f_pos) = rest.find('f') {
        let p = prec(&rest[..f_pos]).unwrap_or(2);
        return format!("{v:.p$}");
    }
    if rest.ends_with('d') {
        return format!("{}", v.round() as i64);
    }
    if let Some(spec) = rest.strip_suffix('m') {
        return sexagesimal(v, prec(spec).unwrap_or(6));
    }
    v.to_string()
}

/// INDI `%m`: `f` ≤ 5 → H:MM, else H:MM:SS with `f`−6 decimals (≤ 2).
/// Rounded in the finest unit so a carry never shows `:60`.
fn sexagesimal(v: f64, f: usize) -> String {
    let sign = if v < 0.0 { "-" } else { "" };
    if f <= 5 {
        let m = (v.abs() * 60.0).round() as i64;
        return format!("{sign}{:02}:{:02}", m / 60, m % 60);
    }
    let d = (f - 6).min(2);
    let k = 10_i64.pow(d as u32);
    let t = (v.abs() * 3600.0 * k as f64).round() as i64;
    let s = t / k;
    let hms = format!("{sign}{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    if d == 0 { hms } else { format!("{hms}.{:0d$}", t % k) }
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
        debug_log!("[devices] giving up enumerating {device} after 60s");
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

    let connected = Memo::new(move |_| {
        let Some(dev) = selected.get() else { return false };
        online.get()
            && devices.with(|ds| {
                ds.iter()
                    .find(|d| d.name == dev)
                    .is_some_and(|d| indi_properties.with(|m| is_connected(m, d)))
            })
    });

    // INDI groups of the selected device in definition order, and the one
    // shown: the picked group while this device has it (so "Main Control"
    // sticks across devices), else the first.
    let groups = Memo::new(move |_| {
        let Some(dev) = selected.get() else { return Vec::new() };
        indi_properties.with(|m| {
            let mut out: Vec<String> = Vec::new();
            for p in m.get(&dev).into_iter().flatten() {
                if !out.contains(&p.group) {
                    out.push(p.group.clone());
                }
            }
            out
        })
    });
    let group_pick: RwSignal<Option<String>> = RwSignal::new(None);
    let group = Memo::new(move |_| {
        groups.with(|gs| group_pick.with(|p| p.as_ref().filter(|g| gs.contains(g)).or(gs.first()).cloned()))
    });
    let props = Memo::new(move |_| {
        let (Some(dev), Some(g)) = (selected.get(), group.get()) else { return Vec::new() };
        indi_properties.with(|m| {
            m.get(&dev).into_iter().flatten().filter(|p| p.group == g).cloned().collect::<Vec<_>>()
        })
    });

    // Newest first (the store appends).
    let messages = Memo::new(move |_| {
        let Some(dev) = selected.get() else { return Vec::new() };
        indi_messages.with(|m| m.get(&dev).map(|v| v.iter().rev().cloned().collect()).unwrap_or_default())
    });
    let messages_open = RwSignal::new(false);

    let send_rows = Arc::clone(&send);

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Devices)></span>
                <span class="min-w-0 truncate font-semibold text-text-blue-bright">{move || tr().tab_devices}</span>
                <Show when=move || selected.with(Option::is_some)>
                    <span class=move || if connected.get() { "badge badge--ok ml-auto shrink-0" } else { "badge ml-auto shrink-0" }>
                        {move || if connected.get() { tr().connected_label } else { tr().disconnected }}
                    </span>
                </Show>
            </div>

            <Show when=move || devices.with(Vec::is_empty)>
                <div class="flex-1 grid place-items-center p-6 text-center text-sm text-text-muted">
                    {move || tr().no_devices}
                </div>
            </Show>

            <div class="flex-1 min-h-0 flex flex-col md:flex-row" class:hidden=move || devices.with(Vec::is_empty)>
                // Devices — chip strip on phones, sidebar from md.
                <div class="shrink-0 flex gap-1.5 overflow-x-auto max-md:[scrollbar-width:none] px-3 py-2 \
                            border-b border-border-base md:w-60 md:flex-col md:overflow-x-hidden \
                            md:overflow-y-auto md:py-3 md:border-b-0 md:border-r">
                    <For
                        each=move || devices.get()
                        key=|d| (d.name.clone(), d.connected, d.interface)
                        children=move |d: DeviceInfo| {
                            let (name, pick, me) = (d.name.clone(), d.name.clone(), d.name.clone());
                            let icon = device_icon(d.interface);
                            view! {
                                <button type="button"
                                        class=move || if selected.with(|s| s.as_deref() == Some(me.as_str())) {
                                            format!("{DEVICE} btn--active")
                                        } else {
                                            DEVICE.to_string()
                                        }
                                        on:click=move |_| selected.set(Some(pick.clone()))>
                                    <span class=move || format!("{LED} {}",
                                        if indi_properties.with(|m| is_connected(m, &d)) { "bg-state-ok" } else { "bg-text-muted" })>
                                    </span>
                                    <span class="inline-block w-4 h-4 shrink-0 text-text-muted" inner_html=icon></span>
                                    <span class="min-w-0 truncate">{name}</span>
                                </button>
                            }
                        }
                    />
                </div>

                <div class="flex-1 min-w-0 min-h-0 flex flex-col">
                    // Group pills — swiped on phones, wrapped from md.
                    <Show when=move || groups.with(|g| !g.is_empty())>
                        <div class="shrink-0 px-3 py-2 md:pl-4 md:pr-6 border-b border-border-base">
                            <div class="max-w-[760px] mx-auto flex gap-1.5 overflow-x-auto \
                                        max-md:[scrollbar-width:none] md:flex-wrap">
                            <For
                                each=move || groups.get()
                                key=|g| g.clone()
                                children=move |g: String| {
                                    let (pick, me) = (g.clone(), g.clone());
                                    view! {
                                        <button type="button"
                                                class=move || if group.with(|c| c.as_deref() == Some(me.as_str())) {
                                                    format!("{CHIP} shrink-0 whitespace-nowrap btn--active")
                                                } else {
                                                    format!("{CHIP} shrink-0 whitespace-nowrap")
                                                }
                                                on:click=move |_| group_pick.set(Some(pick.clone()))>
                                            {g}
                                        </button>
                                    }
                                }
                            />
                            </div>
                        </div>
                    </Show>

                    // Properties of the group, one card each.
                    <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:pl-4 md:pr-6">
                        <div class="max-w-[760px] mx-auto flex flex-col gap-3">
                            <Show when=move || props.with(Vec::is_empty)>
                                <div class="py-8 text-center text-sm text-text-muted">
                                    {move || if online.get() { tr().dev_loading_props } else { tr().disconnected }}
                                </div>
                            </Show>
                            <For
                                each=move || props.get()
                                // Structural key: rebuilds the card when the
                                // compact placeholder is upgraded to the full
                                // record or the element set changes; value
                                // updates keep the DOM (and input focus).
                                key=move |p: &IndiProperty| format!(
                                    "{:?}/{}/{}/{}",
                                    selected.get_untracked(), p.name, p.full, p.elements.len()
                                )
                                children=move |p: IndiProperty| {
                                    view! {
                                        <PropertyRow
                                            device=selected.get_untracked().unwrap_or_default()
                                            snapshot=p
                                            indi_properties=indi_properties
                                            send=Arc::clone(&send_rows)
                                        />
                                    }
                                }
                            />
                        </div>
                    </div>

                    // Latest device message → all of them in a sheet.
                    <Show when=move || messages.with(|m| !m.is_empty())>
                        <button type="button" class=format!("{FOOTER} w-full text-left md:pl-4 md:pr-6")
                                on:click=move |_| messages_open.set(true)>
                            <span class="flex-1 min-w-0 truncate font-mono text-xs text-text-muted">
                                {move || messages.with(|m| m.first().cloned().unwrap_or_default())}
                            </span>
                            <span class="badge shrink-0">{move || messages.with(Vec::len)}</span>
                        </button>
                    </Show>
                </div>
            </div>

            <Show when=move || messages_open.get()>
                {sheet(move || tr().dev_messages_title, move || messages_open.set(false), view! {
                    <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 \
                                flex flex-col gap-1 font-mono text-xs text-text-muted">
                        {move || messages.get().into_iter().map(|m| view! { <div class="break-words">{m}</div> }).collect::<Vec<_>>()}
                    </div>
                })}
            </Show>
        </div>
    }
}

/// One INDI property as a card: state LED + label (+ Set for buffered
/// kinds), then per-element widgets. Structure comes from the `snapshot` the
/// card was keyed on; live values are read reactively from the store so
/// pushed updates refresh in place without rebuilding the DOM.
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

    // Set — send ALL elements (INDI vectors are atomic): pending edits where
    // present, current store values otherwise. Unparseable number input is
    // sent as a string — KStars runs f_scansexa on it (indistd.cpp:1095).
    let on_set = {
        let send = Arc::clone(&send);
        let device = device.clone();
        let name = name.clone();
        move || {
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

    let elements_view: Vec<AnyView> = if snapshot.is_switch() {
        vec![render_switch_property(&device, &snapshot, live, Arc::clone(&send))]
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
        // A form so Enter (the phone's Go key) applies like Set.
        <form class=move || if prop_state.get() == IndiState::Alert { format!("{CARD} border-state-err") } else { CARD.to_string() }
              on:submit=move |ev| { ev.prevent_default(); on_set(); }>
            <div class="flex items-center gap-2 min-h-7">
                <span class=move || format!("{LED} {}", led(prop_state.get()))></span>
                <span class="flex-1 min-w-0 truncate text-sm font-semibold text-text-blue-bright" title=title>
                    {label}
                </span>
                {is_buffered.then(|| view! {
                    <button type="submit"
                            class=move || if edits.with(HashMap::is_empty) {
                                "btn btn-ghost btn--sm max-md:h-9 shrink-0"
                            } else {
                                "btn btn-primary btn--sm max-md:h-9 shrink-0"
                            }
                            disabled=move || prop_state.get() == IndiState::Busy>
                        {move || t(lang.get()).set_btn}
                    </button>
                })}
            </div>
            <div class="flex flex-col">{elements_view}</div>
        </form>
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

/// Buffered input: shows the pending edit, else the live value, so pushes
/// don't stomp typing (Set or a card rebuild clears the buffer). Cyan while
/// edited.
fn edit_input(
    el_name: &str,
    edits: RwSignal<HashMap<String, String>>,
    current: impl Fn() -> String + Send + Sync + 'static,
    inputmode: &'static str,
    width: &'static str,
) -> AnyView {
    let (edited, shown, typed) = (el_name.to_string(), el_name.to_string(), el_name.to_string());
    view! {
        <input type="text" inputmode=inputmode autocomplete="off" autocapitalize="off" spellcheck="false"
               class=move || if edits.with(|e| e.contains_key(&edited)) {
                   format!("{INPUT} {width} border-accent-cyan")
               } else {
                   format!("{INPUT} {width}")
               }
               prop:value=move || edits.with(|e| e.get(&shown).cloned()).unwrap_or_else(&current)
               on:input=move |ev| {
                   let v = event_target_value(&ev);
                   edits.update(|e| {
                       e.insert(typed.clone(), v);
                   });
               } />
    }
    .into_any()
}

/// Number / text / light element row: label left, widget right.
fn render_scalar_element(
    el: &IndiElement,
    writable: bool,
    live: Signal<Option<IndiProperty>>,
    edits: RwSignal<HashMap<String, String>>,
) -> AnyView {
    let el_name = el.name.clone();
    let value = element_value(live, &el_name);

    let widget: AnyView = match &el.value {
        IndiElementValue::Number { min, format, .. } => {
            let fmt = format.clone();
            let current = move || match value() {
                Some(IndiElementValue::Number { value, .. }) => format_indi_number(&fmt, value),
                _ => String::new(),
            };
            if writable {
                // Phone keypads: the decimal pad has no ':' or '-'.
                let mode = if format.contains('m') || *min < 0.0 { "text" } else { "decimal" };
                view! {
                    <span class="shrink-0 font-mono text-sm text-text-muted">{current.clone()}</span>
                    {edit_input(&el_name, edits, current, mode, "w-28 md:w-36 text-right")}
                }
                .into_any()
            } else {
                view! { <span class="shrink-0 font-mono text-sm">{current}</span> }.into_any()
            }
        }
        IndiElementValue::Text(_) => {
            let current = move || match value() {
                Some(IndiElementValue::Text(t)) => t,
                _ => String::new(),
            };
            if writable {
                edit_input(&el_name, edits, current, "text", "w-3/5")
            } else {
                view! { <span class="max-w-[60%] text-sm text-right break-all">{current}</span> }.into_any()
            }
        }
        IndiElementValue::Light(_) => {
            let cls = move || match value() {
                Some(IndiElementValue::Light(s)) => led(s),
                _ => led(IndiState::Idle),
            };
            view! { <span class=move || format!("{LED} {}", cls())></span> }.into_any()
        }
        // Switches are rendered whole-property in render_switch_property.
        IndiElementValue::Switch(_) => ().into_any(),
    };

    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1") title=el_name>{el.label.clone()}</span>
            {widget}
        </div>
    }
    .into_any()
}

/// Whole switch property as one control. 1OFMANY/ATMOST1 → pills (≤6
/// options) or <select>; NOFMANY → checkboxes; read-only → status dots.
/// Writes apply immediately: KStars resets exclusive vectors before applying
/// (indistd.cpp:978), so sending just the target element is enough.
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

    let el_on = move |el: &str| -> bool {
        live.with(|p| {
            p.as_ref()
                .and_then(|p| p.elements.iter().find(|e| e.name == el))
                .is_some_and(|e| matches!(e.value, IndiElementValue::Switch(true)))
        })
    };
    let set = move |el: &str, on: bool| {
        send_device_property_set(
            &send,
            &device,
            &prop_name,
            serde_json::json!([{ "name": el, "state": if on { 1 } else { 0 } }]),
        );
    };

    if !writable {
        let items: Vec<AnyView> = snapshot
            .elements
            .iter()
            .map(|e| {
                let el_name = e.name.clone();
                view! {
                    <span class="flex items-center gap-1.5 text-sm">
                        <span class=move || format!("{LED} {}", if el_on(&el_name) { "bg-state-ok" } else { "bg-text-muted" })></span>
                        {e.label.clone()}
                    </span>
                }
                .into_any()
            })
            .collect();
        return view! { <div class="flex flex-wrap gap-x-4 gap-y-1 py-1">{items}</div> }.into_any();
    }

    if exclusive && snapshot.elements.len() > 6 {
        // Dropdown of element labels; change sends the ON element only.
        let options: Vec<(String, String)> = snapshot
            .elements
            .iter()
            .map(|e| (e.name.clone(), e.label.clone()))
            .collect();
        let names: Vec<String> = options.iter().map(|(n, _)| n.clone()).collect();
        let active = move || names.iter().find(|n| el_on(n)).cloned().unwrap_or_default();
        return view! {
            <select class="input input--sm w-full max-md:h-9" prop:value=active
                    on:change=move |ev| {
                        let sel = event_target_value(&ev);
                        if !sel.is_empty() {
                            set(&sel, true);
                        }
                    }>
                {options
                    .into_iter()
                    .map(|(n, l)| view! { <option value=n>{l}</option> })
                    .collect::<Vec<_>>()}
            </select>
        }
        .into_any();
    }

    let items: Vec<AnyView> = snapshot
        .elements
        .iter()
        .map(|e| {
            let el_name = e.name.clone();
            let label = e.label.clone();
            let set = set.clone();
            if exclusive {
                let shown = el_name.clone();
                view! {
                    <button type="button"
                            class=move || if el_on(&shown) {
                                format!("{CHIP} flex-1 whitespace-nowrap btn--active")
                            } else {
                                format!("{CHIP} flex-1 whitespace-nowrap")
                            }
                            // ATMOST1 allows all-off: re-clicking the active
                            // element turns it off.
                            on:click=move |_| set(&el_name, !(at_most_one && el_on(&el_name)))>
                        {label}
                    </button>
                }
                .into_any()
            } else {
                let shown = el_name.clone();
                view! {
                    <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                        <input type="checkbox" class=CHECK
                               prop:checked=move || el_on(&shown)
                               on:change=move |ev| set(&el_name, event_target_checked(&ev)) />
                        <span class=LABEL>{label}</span>
                    </label>
                }
                .into_any()
            }
        })
        .collect();

    if exclusive {
        view! { <div class="flex flex-wrap gap-1.5 py-1">{items}</div> }.into_any()
    } else {
        view! { <div class="flex flex-col">{items}</div> }.into_any()
    }
}
