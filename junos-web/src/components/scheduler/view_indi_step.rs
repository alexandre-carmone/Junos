//! Scheduler: the body of a custom INDI step card ("Set an INDI property" /
//! "Wait for an INDI property") in the startup/shutdown queue editor.
//!
//! When Ekos lists the device's properties (`device_get`, the reply the
//! Devices tab reads), the card picks from them — device, property grouped as
//! in the Devices tab, element — and shows the form that fits the property:
//! - a number: a field with its range, next to its current value;
//! - a text: a text field;
//! - a switch: one of many is a list, so picking the option is the whole
//!   choice (the step turns it on); a lone switch is a button press; any of
//!   many gets On / Off;
//! - a light (Wait only): On / Off;
//! - the property's state (Wait only): Idle / OK / Busy / Alert.
//!
//! Without a list — Ekos offline, a name the device doesn't have, or "Type
//! names" — the fields are free text with suggestions and a type picker.

use std::collections::HashMap;

use leptos::prelude::*;

use super::labels::{cond_label, kind_label};
use super::queue_model::{fmt_num, FAIL_ABORT, FAIL_CONTINUE};
use super::queue_native::{Cond, IndiKind, IndiOp, IndiStep, CONDS, INDI_MARGIN, INDI_RETRIES, INDI_TIMEOUT, KINDS};
use super::view_queue_editor::{input_cls, number_field, FIELDS, FIELD_LABEL, SELECT};
use crate::components::form::{CHECK, CHIP};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang};
use crate::ws::{DeviceInfo, IndiElementValue, IndiPerm, IndiProperty, IndiRule, IndiState, SendCmd};
use crate::ws_helpers::send_cmd;

/// The element picker's "property state" entry.
const STATE: &str = "STATE";
const PICK: &str = "input input--sm w-full min-w-0 max-md:h-9";
const SMALL: &str = "text-xs text-text-muted";

// ── Live INDI data ───────────────────────────────────────────────────────────

/// The live INDI data the cards pick from.
#[derive(Clone, Copy)]
pub(super) struct IndiSource {
    pub devices: RwSignal<Vec<DeviceInfo>>,
    pub props: RwSignal<HashMap<String, Vec<IndiProperty>>>,
    pub send: StoredValue<SendCmd>,
    /// Devices already asked for their property list.
    pub asked: StoredValue<Vec<String>>,
}

/// What the form needs of a property: its shape, not its values — so a value
/// pushed by the driver doesn't rebuild the pickers.
#[derive(Clone, PartialEq)]
struct PropShape {
    name: String,
    label: String,
    group: String,
    kind: IndiKind,
    /// `SetAction` can write it.
    writable: bool,
    rule: IndiRule,
    elements: Vec<ElemShape>,
}

#[derive(Clone, PartialEq)]
struct ElemShape {
    name: String,
    label: String,
    /// A number's min and max, when the driver gives a real range.
    range: Option<(f64, f64)>,
}

impl PropShape {
    fn of(p: &IndiProperty) -> Option<Self> {
        let kind = kind_of(p)?;
        Some(Self {
            name: p.name.clone(),
            label: p.label.clone(),
            group: p.group.clone(),
            kind,
            writable: p.perm != IndiPerm::Ro && kind.settable(),
            rule: p.rule,
            elements: p
                .elements
                .iter()
                .map(|e| ElemShape {
                    name: e.name.clone(),
                    label: e.label.clone(),
                    range: match e.value {
                        IndiElementValue::Number { min, max, .. } if min < max => Some((min, max)),
                        _ => None,
                    },
                })
                .collect(),
        })
    }

    /// Setting it means picking one option (or pressing the only one):
    /// `SetAction` turns the others off for a one-of-many switch.
    fn is_choice(&self) -> bool {
        self.kind == IndiKind::Switch && (self.rule == IndiRule::OneOfMany || self.elements.len() == 1)
    }
}

fn kind_of(p: &IndiProperty) -> Option<IndiKind> {
    Some(match p.elements.first()?.value {
        IndiElementValue::Number { .. } => IndiKind::Number,
        IndiElementValue::Text(_) => IndiKind::Text,
        IndiElementValue::Switch(_) => IndiKind::Switch,
        IndiElementValue::Light(_) => IndiKind::Light,
    })
}

fn state_name(s: IndiState) -> &'static str {
    match s {
        IndiState::Idle => "Idle",
        IndiState::Ok => "OK",
        IndiState::Busy => "Busy",
        IndiState::Alert => "Alert",
    }
}

/// `Open (SHUTTER_OPEN)`, or just the name when the label adds nothing.
fn option_label(name: &str, label: &str) -> String {
    if label.is_empty() || label == name { name.to_string() } else { format!("{label} ({name})") }
}

/// Properties by INDI group, in the driver's order.
fn grouped(shapes: &[PropShape], writable_only: bool) -> Vec<(String, Vec<PropShape>)> {
    let mut groups: Vec<(String, Vec<PropShape>)> = Vec::new();
    for p in shapes.iter().filter(|p| !writable_only || p.writable) {
        match groups.iter_mut().find(|(g, _)| *g == p.group) {
            Some((_, v)) => v.push(p.clone()),
            None => groups.push((p.group.clone(), vec![p.clone()])),
        }
    }
    groups
}

impl IndiSource {
    fn device_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.devices.with(|v| v.iter().map(|d| d.name.clone()).collect());
        self.props.with(|m| names.extend(m.keys().cloned()));
        names.sort();
        names.dedup();
        names
    }

    /// The device's properties, once Ekos has listed them in full.
    fn shapes(&self, device: &str) -> Option<Vec<PropShape>> {
        self.props.with(|m| {
            let v = m.get(device.trim())?;
            v.iter().any(|p| p.full).then(|| v.iter().filter_map(PropShape::of).collect())
        })
    }

    fn property(&self, device: &str, name: &str) -> Option<IndiProperty> {
        self.props.with_untracked(|m| m.get(device.trim())?.iter().find(|p| p.name == name.trim()).cloned())
    }

    /// What the element (or the property's state) reads now.
    fn current(&self, device: &str, property: &str, element: &str, kind: IndiKind) -> Option<String> {
        self.props.with(|m| {
            let p = m.get(device.trim())?.iter().find(|p| p.name == property.trim())?;
            if kind == IndiKind::State {
                return Some(state_name(p.state).to_string());
            }
            let e = p.elements.iter().find(|e| e.name == element.trim())?;
            Some(match &e.value {
                IndiElementValue::Number { value, .. } => fmt_num((value * 1e6).round() / 1e6),
                IndiElementValue::Text(s) => s.clone(),
                IndiElementValue::Switch(on) => (if *on { "On" } else { "Off" }).to_string(),
                IndiElementValue::Light(s) => state_name(*s).to_string(),
            })
        })
    }

    /// Ask KStars once for a known device's properties, unless they're in.
    /// Reads `devices` tracked, so an Effect retries when Ekos lists them.
    fn enumerate(&self, device: &str) {
        let known = self.devices.with(|v| v.iter().any(|d| d.name == device));
        let listed = self.props.with_untracked(|m| m.get(device).is_some_and(|v| v.iter().any(|p| p.full)));
        if !known || listed || self.asked.with_value(|a| a.iter().any(|d| d == device)) {
            return;
        }
        self.asked.update_value(|a| a.push(device.to_string()));
        self.send.with_value(|send| {
            send_cmd(send, "device_get", serde_json::json!({ "device": device, "compact": false }));
        });
    }
}

// ── Row ──────────────────────────────────────────────────────────────────────

/// One signal per field, like the editor's other rows, so typing never
/// rebuilds the card.
#[derive(Clone, Copy)]
pub(super) struct IndiRow {
    pub op: RwSignal<IndiOp>,
    device: RwSignal<String>,
    property: RwSignal<String>,
    element: RwSignal<String>,
    kind: RwSignal<IndiKind>,
    cond: RwSignal<Cond>,
    value: RwSignal<String>,
    margin: RwSignal<String>,
    wait_done: RwSignal<bool>,
    timeout: RwSignal<String>,
    retries: RwSignal<String>,
    on_fail: RwSignal<u8>,
}

impl IndiRow {
    pub fn new(s: IndiStep) -> Self {
        Self {
            op: RwSignal::new(s.op),
            device: RwSignal::new(s.device),
            property: RwSignal::new(s.property),
            element: RwSignal::new(s.element),
            kind: RwSignal::new(s.kind),
            cond: RwSignal::new(s.cond),
            value: RwSignal::new(s.value),
            margin: RwSignal::new(s.margin),
            wait_done: RwSignal::new(s.wait_done),
            timeout: RwSignal::new(s.timeout),
            retries: RwSignal::new(s.retries),
            on_fail: RwSignal::new(s.on_fail),
        }
    }

    pub fn snapshot(&self) -> IndiStep {
        IndiStep {
            op: self.op.get_untracked(),
            device: self.device.get_untracked(),
            property: self.property.get_untracked(),
            element: self.element.get_untracked(),
            kind: self.kind.get_untracked(),
            cond: self.cond.get_untracked(),
            value: self.value.get_untracked(),
            margin: self.margin.get_untracked(),
            wait_done: self.wait_done.get_untracked(),
            timeout: self.timeout.get_untracked(),
            retries: self.retries.get_untracked(),
            on_fail: self.on_fail.get_untracked(),
        }
    }

    /// Switch kind, keeping the condition and value valid for it.
    fn set_kind(&self, kind: IndiKind) {
        self.kind.set(kind);
        if !kind.conds().contains(&self.cond.get_untracked()) {
            self.cond.set(Cond::Eq);
        }
        if let Some(choices) = kind.choices() {
            if !choices.contains(&self.value.get_untracked().trim()) {
                self.value.set(choices[0].to_string());
            }
        }
    }

    /// A property picked from the list: its first element, and a value that fits.
    fn pick_property(&self, shape: &PropShape, source: IndiSource) {
        self.property.set(shape.name.clone());
        self.cond.set(Cond::Eq);
        let first = shape.elements.first().map(|e| e.name.clone()).unwrap_or_default();
        self.pick_element(shape, &first, source);
    }

    /// An element (or [`STATE`]) picked: numbers and texts start from what
    /// the device reads now, a choice from On.
    fn pick_element(&self, shape: &PropShape, element: &str, source: IndiSource) {
        if element == STATE {
            self.element.set(String::new());
            self.set_kind(IndiKind::State);
            return;
        }
        self.element.set(element.to_string());
        self.set_kind(shape.kind);
        match shape.kind {
            IndiKind::Number | IndiKind::Text => {
                let now = source.current(&self.device.get_untracked(), &shape.name, element, shape.kind);
                if let Some(v) = now {
                    self.value.set(v);
                }
            }
            _ if shape.is_choice() && self.op.get_untracked() == IndiOp::Set => self.value.set("On".to_string()),
            _ => {}
        }
    }

    fn clear_property(&self) {
        self.property.set(String::new());
        self.element.set(String::new());
    }
}

// ── Pieces ───────────────────────────────────────────────────────────────────

fn chip(active: bool) -> String {
    if active { format!("{CHIP} btn--active") } else { CHIP.to_string() }
}

fn labelled(label: impl Fn() -> &'static str + Send + 'static, control: impl IntoView) -> impl IntoView {
    view! {
        <label class="flex flex-col gap-1 min-w-0">
            <span class=SMALL>{move || label()}</span>
            {control}
        </label>
    }
}

/// A free-text field with suggestions from datalist `list`.
fn suggest_field(
    label: impl Fn() -> &'static str + Send + 'static,
    list_id: String,
    value: RwSignal<String>,
    on_commit: impl Fn(String) + 'static,
) -> impl IntoView {
    labelled(label, view! {
        <input
            class="input input--sm font-mono w-full max-md:h-9"
            list=list_id
            autocomplete="off"
            autocapitalize="off"
            spellcheck="false"
            prop:value=move || value.get()
            on:input=move |ev| value.set(event_target_value(&ev))
            on:change=move |ev| on_commit(event_target_value(&ev))
        />
    })
}

fn datalist(id: String, options: impl Fn() -> Vec<(String, String)> + Send + 'static) -> impl IntoView {
    view! {
        <datalist id=id>
            {move || options().into_iter().map(|(value, label)| view! { <option value=value>{label}</option> }).collect_view()}
        </datalist>
    }
}

/// One pill per allowed value.
fn pills(value: RwSignal<String>, choices: &'static [&'static str]) -> impl IntoView {
    view! {
        <div class="flex flex-wrap gap-1.5">
            {choices.iter().map(|c| view! {
                <button type="button" class=move || chip(value.with(|v| v.trim() == *c))
                        on:click=move |_| value.set(c.to_string())>
                    {*c}
                </button>
            }).collect_view()}
        </div>
    }
}

/// The Wait condition, among those the kind supports.
fn cond_select(row: IndiRow, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    view! {
        <select
            class=SELECT
            on:change=move |ev| {
                let i = event_target_value(&ev).parse::<usize>().unwrap_or(0);
                row.cond.set(CONDS.get(i).copied().unwrap_or(Cond::Eq));
            }
        >
            {move || row.kind.get().conds().iter().map(|c| {
                let c = *c;
                view! {
                    <option value=(c as usize).to_string() prop:selected=move || row.cond.get() == c>
                        {move || cond_label(tr(), c)}
                    </option>
                }
            }).collect_view()}
        </select>
    }
}

/// A number or text value; a number is outlined red outside `range`.
fn value_input(row: IndiRow, lang: RwSignal<Lang>, range: Option<(f64, f64)>) -> impl IntoView {
    let tr = move || t(lang.get());
    let number = row.kind.get_untracked() == IndiKind::Number;
    let ok = move || {
        !number
            || row.value.with(|v| {
                v.trim()
                    .parse::<f64>()
                    .is_ok_and(|n| n.is_finite() && range.is_none_or(|(lo, hi)| n >= lo && n <= hi))
            })
    };
    let hint = range.map(|(lo, hi)| format!("{} … {}", fmt_num(lo), fmt_num(hi))).unwrap_or_default();
    view! {
        <input
            class=move || input_cls("w-[160px] max-md:flex-1", ok())
            inputmode=if number { "decimal" } else { "text" }
            placeholder=hint.clone()
            title=hint
            aria-label=move || tr().sched_q_indi_value
            prop:value=move || row.value.get()
            on:input=move |ev| row.value.set(event_target_value(&ev))
        />
    }
}

// ── Card body ────────────────────────────────────────────────────────────────

pub(super) fn indi_body(row: IndiRow, key: u32, lang: RwSignal<Lang>, source: IndiSource) -> impl IntoView {
    let tr = move || t(lang.get());

    Effect::new(move |_| source.enumerate(row.device.get().trim()));

    // "Type names" forces the free-text fields.
    let manual = RwSignal::new(false);
    let names = Memo::new(move |_| source.device_names());
    let listing = Memo::new(move |_| source.shapes(&row.device.get()));
    let shape = Memo::new(move |_| {
        let name = row.property.get();
        listing.with(|l| l.as_ref()?.iter().find(|p| p.name == name.trim()).cloned())
    });
    let picking = Memo::new(move |_| !manual.get() && names.with(|n| !n.is_empty()));
    let live = Memo::new(move |_| {
        picking.get() && listing.with(Option::is_some) && (row.property.with(|p| p.trim().is_empty()) || shape.with(Option::is_some))
    });
    let toggle_manual = move |_| {
        if manual.get_untracked() {
            manual.set(false);
            if shape.with_untracked(Option::is_none) {
                row.clear_property();
            }
        } else {
            manual.set(true);
        }
    };

    let set_op = move |op: IndiOp| {
        row.op.set(op);
        if op != IndiOp::Set {
            return;
        }
        // KStars can't set a light, a read-only property or a state.
        match shape.get_untracked() {
            Some(sh) if !sh.writable => row.clear_property(),
            Some(sh) if row.kind.get_untracked() == IndiKind::State => {
                let first = sh.elements.first().map(|e| e.name.clone()).unwrap_or_default();
                row.pick_element(&sh, &first, source);
            }
            _ if !row.kind.get_untracked().settable() => row.set_kind(IndiKind::Switch),
            _ => {}
        }
    };
    let op_pill = move |op: IndiOp, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button type="button" class=move || chip(row.op.get() == op)
                aria-pressed=move || (row.op.get() == op).to_string()
                on:click=move |_| set_op(op)>
            {move || label(tr())}
        </button>
    };

    // ── Device: a list while Ekos reports devices, else free text ──
    let dev_list = format!("q-dev-{key}");
    let device_field = move || {
        if picking.get() {
            labelled(move || tr().sched_q_indi_device, view! {
                <select class=PICK on:change=move |ev| {
                    let device = event_target_value(&ev);
                    if device != row.device.get_untracked() {
                        row.device.set(device);
                        row.clear_property();
                    }
                }>
                    <option value="" prop:selected=move || row.device.with(|d| d.trim().is_empty())>
                        {move || tr().sched_q_indi_pick}
                    </option>
                    {move || {
                        let current = row.device.get().trim().to_string();
                        let mut list = names.get();
                        if !current.is_empty() && !list.contains(&current) {
                            list.push(current);
                        }
                        list.into_iter().map(|d| {
                            let (value, this) = (d.clone(), d.clone());
                            view! {
                                <option value=value prop:selected=move || row.device.with(|x| x.trim() == this)>{d}</option>
                            }
                        }).collect_view()
                    }}
                </select>
            }).into_any()
        } else {
            suggest_field(move || tr().sched_q_indi_device, dev_list.clone(), row.device, |_| {}).into_any()
        }
    };

    // ── Property and element picked from the device's list ──
    let live_pickers = move || {
        let property = labelled(move || tr().sched_q_indi_property, view! {
            <select class=PICK on:change=move |ev| {
                let name = event_target_value(&ev);
                match listing.with_untracked(|l| l.as_ref()?.iter().find(|p| p.name == name).cloned()) {
                    Some(sh) => row.pick_property(&sh, source),
                    None => row.clear_property(),
                }
            }>
                <option value="" prop:selected=move || row.property.with(|p| p.trim().is_empty())>
                    {move || tr().sched_q_indi_pick}
                </option>
                {move || {
                    let writable_only = row.op.get() == IndiOp::Set;
                    listing.with(|l| grouped(l.as_deref().unwrap_or(&[]), writable_only)).into_iter().map(|(group, props)| view! {
                        <optgroup label=group>
                            {props.into_iter().map(|p| {
                                let this = p.name.clone();
                                view! {
                                    <option value=p.name.clone() prop:selected=move || row.property.with(|x| x.trim() == this)>
                                        {option_label(&p.name, &p.label)}
                                    </option>
                                }
                            }).collect_view()}
                        </optgroup>
                    }).collect_view()
                }}
            </select>
        });
        // A Set of a one-element property has nothing to pick.
        let element = move || {
            let wait = row.op.get() == IndiOp::Wait;
            shape.get().filter(|sh| wait || sh.elements.len() > 1).map(|sh| {
                let label = if sh.is_choice() { tr().sched_q_indi_choice } else { tr().sched_q_indi_element };
                let options = sh.elements.clone();
                labelled(move || label, view! {
                    <select class=PICK on:change=move |ev| row.pick_element(&sh, &event_target_value(&ev), source)>
                        {wait.then(|| view! {
                            <option value=STATE prop:selected=move || row.kind.get() == IndiKind::State>
                                {move || tr().sched_q_kind_state}
                            </option>
                        })}
                        {options.into_iter().map(|e| {
                            let this = e.name.clone();
                            view! {
                                <option value=e.name.clone()
                                        prop:selected=move || row.kind.get() != IndiKind::State && row.element.with(|x| x.trim() == this)>
                                    {option_label(&e.name, &e.label)}
                                </option>
                            }
                        }).collect_view()}
                    </select>
                })
            })
        };
        view! { {property} {element} }
    };

    // ── The value, in the form the property calls for ──
    let now = move || {
        let (device, property, element) = (row.device.get(), row.property.get(), row.element.get());
        source
            .current(&device, &property, &element, row.kind.get())
            .map(|v| view! { <span class="text-xs text-text-faint font-mono">{format!("{} {v}", tr().sched_q_indi_now)}</span> })
    };
    let live_value = move || {
        let Some(sh) = shape.get() else { return ().into_any() };
        let wait = row.op.get() == IndiOp::Wait;
        match row.kind.get() {
            IndiKind::State => view! { {cond_select(row, lang)} {pills(row.value, &["Idle", "OK", "Busy", "Alert"])} {now} }.into_any(),
            kind @ (IndiKind::Number | IndiKind::Text) => {
                let range = if kind == IndiKind::Number {
                    row.element.with(|el| sh.elements.iter().find(|e| e.name == el.trim()).and_then(|e| e.range))
                } else {
                    None
                };
                view! { {wait.then(|| cond_select(row, lang))} {value_input(row, lang, range)} {now} }.into_any()
            }
            IndiKind::Switch | IndiKind::Light => {
                if !wait && sh.is_choice() && row.value.with(|v| v.trim() == "On") {
                    let what = if sh.elements.len() == 1 { tr().sched_q_indi_press } else { tr().sched_q_indi_turns_on };
                    view! { <span class="text-sm text-text-muted">{what}</span> {now} }.into_any()
                } else {
                    view! { {pills(row.value, &["On", "Off"])} {now} }.into_any()
                }
            }
        }
    };

    // ── Free text: names typed, the type picked by hand ──
    let (prop_list, el_list) = (format!("q-prop-{key}"), format!("q-el-{key}"));
    let manual_fields = {
        let (prop_list, el_list) = (prop_list.clone(), el_list.clone());
        move || {
            // A known property still fills in its type.
            let on_property = move |name: String| {
                let Some(p) = source.property(&row.device.get_untracked(), &name) else { return };
                if row.kind.get_untracked() != IndiKind::State {
                    if let Some(kind) = kind_of(&p) {
                        row.set_kind(kind);
                    }
                }
            };
            let el_list = el_list.clone();
            view! {
                {suggest_field(move || tr().sched_q_indi_property, prop_list.clone(), row.property, on_property)}
                <Show when=move || row.kind.get() != IndiKind::State>
                    {suggest_field(move || tr().sched_q_indi_element, el_list.clone(), row.element, |_| {})}
                </Show>
            }
        }
    };
    let manual_value = move || {
        let kinds = view! {
            <select
                class=SELECT
                aria-label=move || tr().sched_q_indi_kind
                on:change=move |ev| {
                    if let Some(k) = event_target_value(&ev).parse::<usize>().ok().and_then(|i| KINDS.get(i)) {
                        row.set_kind(*k);
                    }
                }
            >
                {move || KINDS
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| row.op.get() == IndiOp::Wait || k.settable())
                    .map(|(i, k)| {
                        let k = *k;
                        view! {
                            <option value=i.to_string() prop:selected=move || row.kind.get() == k>
                                {move || kind_label(tr(), k)}
                            </option>
                        }
                    })
                    .collect_view()}
            </select>
        };
        let value = move || match row.kind.get().choices() {
            Some(choices) => pills(row.value, choices).into_any(),
            None => value_input(row, lang, None).into_any(),
        };
        view! {
            {kinds}
            <Show when=move || row.op.get() == IndiOp::Wait>{cond_select(row, lang)}</Show>
            {value}
        }
    };

    // Suggestions for the free-text fields.
    let devices = move || names.get().into_iter().map(|d| (d, String::new())).collect::<Vec<_>>();
    let properties = move || {
        listing.with(|l| {
            l.as_deref()
                .unwrap_or(&[])
                .iter()
                .filter(|p| row.op.get() == IndiOp::Wait || p.writable)
                .map(|p| (p.name.clone(), p.label.clone()))
                .collect::<Vec<_>>()
        })
    };
    let elements = move || {
        shape.with(|sh| {
            sh.as_ref()
                .map(|sh| sh.elements.iter().map(|e| (e.name.clone(), e.label.clone())).collect())
                .unwrap_or_default()
        })
    };

    view! {
        <div class="flex flex-col gap-2">
            <div class="flex items-center gap-1.5">
                {op_pill(IndiOp::Set, |tr| tr.sched_q_indi_set)}
                {op_pill(IndiOp::Wait, |tr| tr.sched_q_indi_wait)}
                <Show when=move || names.with(|n| !n.is_empty())>
                    <button type="button" class=format!("{CHIP} ml-auto") on:click=toggle_manual>
                        {move || if manual.get() { tr().sched_q_indi_from_device } else { tr().sched_q_indi_type_names }}
                    </button>
                </Show>
            </div>

            <div class="grid grid-cols-1 sm:grid-cols-3 gap-2">
                {device_field}
                {move || if live.get() { live_pickers().into_any() } else { manual_fields().into_any() }}
            </div>
            {datalist(format!("q-dev-{key}"), devices)}
            {datalist(prop_list, properties)}
            {datalist(el_list, elements)}
            <Show when=move || names.with(Vec::is_empty)>
                <div class="text-text-faint text-xs">{move || tr().sched_q_indi_offline}</div>
            </Show>

            <div class=FIELDS>
                {move || if live.get() { live_value.into_any() } else { manual_value().into_any() }}
                <Show when=move || row.op.get() == IndiOp::Wait && row.cond.get() == Cond::Within>
                    {number_field(lang, &INDI_MARGIN, row.margin)}
                </Show>
            </div>

            <div class=FIELDS>
                {number_field(lang, &INDI_TIMEOUT, row.timeout)}
                {number_field(lang, &INDI_RETRIES, row.retries)}
                <label class="flex items-center gap-sp-1">
                    <span class=format!("{FIELD_LABEL} min-w-0")>{move || tr().sched_q_on_fail}</span>
                    <select class=SELECT on:change=move |ev| row.on_fail.set(event_target_value(&ev).parse().unwrap_or(FAIL_ABORT))>
                        <option value=FAIL_ABORT.to_string() prop:selected=move || row.on_fail.get() == FAIL_ABORT>
                            {move || tr().sched_q_fail_abort}
                        </option>
                        <option value=FAIL_CONTINUE.to_string() prop:selected=move || row.on_fail.get() == FAIL_CONTINUE>
                            {move || tr().sched_q_fail_next}
                        </option>
                    </select>
                </label>
                <Show when=move || row.op.get() == IndiOp::Set>
                    <label class="flex items-center gap-2 cursor-pointer">
                        <input type="checkbox" class=CHECK
                               prop:checked=move || row.wait_done.get()
                               on:change=move |ev| row.wait_done.set(event_target_checked(&ev)) />
                        <span class=FIELD_LABEL>{move || tr().sched_q_indi_wait_done}</span>
                    </label>
                </Show>
            </div>
        </div>
    }
}
