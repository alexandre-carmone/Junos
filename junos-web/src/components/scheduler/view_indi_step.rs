//! Scheduler: the body of a custom INDI step card ("Set an INDI property" /
//! "Wait for an INDI property") in the startup/shutdown queue editor.
//!
//! Device, property and element are free text with suggestions — the devices
//! Ekos reports and, once one is picked, its properties, enumerated with
//! `device_get` (the reply the Devices tab reads). Picking a known property
//! fills in its type, so the value control and the conditions match it.

use std::collections::HashMap;

use leptos::prelude::*;

use super::labels::{cond_label, kind_label};
use super::queue_model::{FAIL_ABORT, FAIL_CONTINUE};
use super::queue_native::{Cond, IndiKind, IndiOp, IndiStep, CONDS, INDI_MARGIN, INDI_RETRIES, INDI_TIMEOUT, KINDS};
use super::view_queue_editor::{input_cls, number_field, FIELDS, FIELD_LABEL, SELECT};
use crate::components::form::{CHECK, CHIP};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang};
use crate::ws::{DeviceInfo, IndiElementValue, IndiPerm, IndiProperty, SendCmd};
use crate::ws_helpers::send_cmd;

/// The live INDI data the cards suggest names from.
#[derive(Clone)]
pub(super) struct IndiSource {
    pub devices: RwSignal<Vec<DeviceInfo>>,
    pub props: RwSignal<HashMap<String, Vec<IndiProperty>>>,
    pub send: SendCmd,
    /// Devices already asked for their property list.
    pub asked: StoredValue<Vec<String>>,
}

impl IndiSource {
    fn device_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.devices.with(|v| v.iter().map(|d| d.name.clone()).collect());
        self.props.with(|m| names.extend(m.keys().cloned()));
        names.sort();
        names.dedup();
        names
    }

    fn property(&self, device: &str, name: &str) -> Option<IndiProperty> {
        self.props.with_untracked(|m| m.get(device.trim())?.iter().find(|p| p.name == name.trim()).cloned())
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
        send_cmd(&self.send, "device_get", serde_json::json!({ "device": device, "compact": false }));
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
}

fn chip(active: bool) -> String {
    if active { format!("{CHIP} btn--active") } else { CHIP.to_string() }
}

/// A labelled text field with suggestions from `list`.
fn suggest_field(
    label: impl Fn() -> &'static str + Send + 'static,
    list_id: String,
    value: RwSignal<String>,
    on_commit: impl Fn(String) + 'static,
) -> impl IntoView {
    view! {
        <label class="flex flex-col gap-1 min-w-0">
            <span class="text-xs text-text-muted">{move || label()}</span>
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
        </label>
    }
}

fn datalist(id: String, options: impl Fn() -> Vec<(String, String)> + Send + 'static) -> impl IntoView {
    view! {
        <datalist id=id>
            {move || options().into_iter().map(|(value, label)| view! { <option value=value>{label}</option> }).collect_view()}
        </datalist>
    }
}

pub(super) fn indi_body(row: IndiRow, key: u32, lang: RwSignal<Lang>, source: IndiSource) -> impl IntoView {
    let tr = move || t(lang.get());
    let (dev_list, prop_list, el_list) = (format!("q-dev-{key}"), format!("q-prop-{key}"), format!("q-el-{key}"));
    let el_field = el_list.clone();

    {
        let source = source.clone();
        Effect::new(move |_| source.enumerate(row.device.get().trim()));
    }

    // Suggestions. A Set only offers what `SetAction` can write.
    let devices = {
        let source = source.clone();
        move || source.device_names().into_iter().map(|d| (d, String::new())).collect::<Vec<_>>()
    };
    let properties = {
        let source = source.clone();
        move || {
            let set = row.op.get() == IndiOp::Set;
            source.props.with(|m| {
                m.get(row.device.get().trim())
                    .map(|v| {
                        v.iter()
                            .filter(|p| !set || (p.perm != IndiPerm::Ro && kind_of(p).is_some_and(IndiKind::settable)))
                            .map(|p| (p.name.clone(), p.label.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            })
        }
    };
    let elements = {
        let source = source.clone();
        move || {
            source.props.with(|m| {
                m.get(row.device.get().trim())
                    .and_then(|v| v.iter().find(|p| p.name == row.property.get().trim()))
                    .map(|p| p.elements.iter().map(|e| (e.name.clone(), e.label.clone())).collect())
                    .unwrap_or_default()
            })
        }
    };

    // A known property fills in the type, and its element when it has one.
    let on_property = {
        let source = source.clone();
        move |name: String| {
            let Some(p) = source.property(&row.device.get_untracked(), &name) else { return };
            if row.kind.get_untracked() != IndiKind::State {
                if let Some(kind) = kind_of(&p) {
                    row.set_kind(kind);
                }
            }
            let element = row.element.get_untracked();
            if p.elements.len() == 1 {
                row.element.set(p.elements[0].name.clone());
            } else if !p.elements.iter().any(|e| e.name == element.trim()) {
                row.element.set(String::new());
            }
        }
    };

    let set_op = move |op: IndiOp| {
        row.op.set(op);
        if op == IndiOp::Set && !row.kind.get_untracked().settable() {
            row.set_kind(IndiKind::Switch);
        }
    };
    let op_pill = move |op: IndiOp, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button type="button" class=move || chip(row.op.get() == op)
                aria-pressed=move || (row.op.get() == op).to_string()
                on:click=move |_| set_op(op)>
            {move || label(tr())}
        </button>
    };

    let offline = {
        let source = source.clone();
        move || source.devices.with(Vec::is_empty) && source.props.with(HashMap::is_empty)
    };

    let value_control = move || match row.kind.get().choices() {
        Some(choices) => view! {
            <div class="flex flex-wrap gap-1.5">
                {choices.iter().map(|c| view! {
                    <button type="button" class=move || chip(row.value.with(|v| v.trim() == *c))
                            on:click=move |_| row.value.set(c.to_string())>
                        {*c}
                    </button>
                }).collect_view()}
            </div>
        }.into_any(),
        None => {
            let number = row.kind.get() == IndiKind::Number;
            let ok = move || !number || row.value.with(|v| v.trim().parse::<f64>().is_ok_and(f64::is_finite));
            view! {
                <input
                    class=move || input_cls("w-[160px] max-md:flex-1", ok())
                    inputmode=if number { "decimal" } else { "text" }
                    aria-label=move || tr().sched_q_indi_value
                    prop:value=move || row.value.get()
                    on:input=move |ev| row.value.set(event_target_value(&ev))
                />
            }.into_any()
        }
    };

    view! {
        <div class="flex flex-col gap-2">
            <div class="flex gap-1.5">
                {op_pill(IndiOp::Set, |tr| tr.sched_q_indi_set)}
                {op_pill(IndiOp::Wait, |tr| tr.sched_q_indi_wait)}
            </div>

            <div class="grid grid-cols-1 sm:grid-cols-3 gap-2">
                {suggest_field(move || tr().sched_q_indi_device, dev_list.clone(), row.device, |_| {})}
                {suggest_field(move || tr().sched_q_indi_property, prop_list.clone(), row.property, on_property)}
                <Show when=move || row.kind.get() != IndiKind::State>
                    {suggest_field(move || tr().sched_q_indi_element, el_field.clone(), row.element, |_| {})}
                </Show>
            </div>
            {datalist(dev_list, devices)}
            {datalist(prop_list, properties)}
            {datalist(el_list, elements)}
            <Show when=offline>
                <div class="text-text-faint text-xs">{move || tr().sched_q_indi_offline}</div>
            </Show>

            <div class=FIELDS>
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
                <Show when=move || row.op.get() == IndiOp::Wait>
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
                </Show>
                {value_control}
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
