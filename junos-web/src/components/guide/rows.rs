//! Setting rows shared by the Guide tab's two backends: a checkbox, a number
//! and a combo, each reading its value by key from one JSON map and writing
//! it back through the backend's own setter (Ekos' `guide_set_all_settings`,
//! junos-guider's `set_profile_value`), plus the collapsible card they sit in.

use std::sync::Arc;

use leptos::prelude::*;
use serde_json::Value;

use crate::components::form::{CARD_TITLE, CHECK, LABEL, ROW};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};

pub type LabelFn = fn(&Translations) -> &'static str;

pub const NUM: &str = "input input--sm font-mono w-[104px] shrink-0 text-right max-md:h-9";
const SELECT: &str = "input input--sm w-[180px] shrink-0 max-md:h-9";

/// What every row needs: the values (one map, read by key), how to write
/// one back, and the language.
#[derive(Clone)]
pub struct Rows {
    pub values: Signal<Value>,
    pub set: Arc<dyn Fn(&'static str, Value) + Send + Sync>,
    pub lang: RwSignal<Lang>,
}

impl Rows {
    pub fn label(&self, label: LabelFn) -> impl Fn() -> &'static str + Copy + Send + 'static + use<> {
        let lang = self.lang;
        move || label(t(lang.get()))
    }
}

/// A value as the text a combo or a text field shows.
pub fn value_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| match x {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

/// A boolean as the profile holds it: `true`/`false`, or `1`/`0` in a
/// profile imported from PHD2's text, which types every entry as a number.
pub fn as_flag(v: &Value) -> Option<bool> {
    v.as_bool().or(v.as_f64().map(|x| x != 0.0))
}

/// Checkbox row for a boolean.
pub fn bool_row(r: &Rows, key: &'static str, label: LabelFn) -> impl IntoView + use<> {
    let (set, values) = (r.set.clone(), r.values);
    view! {
        <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
            <input type="checkbox" class=CHECK
                   prop:checked=move || values.with(|s| s.get(key).and_then(as_flag).unwrap_or(false))
                   on:change=move |ev| set(key, Value::Bool(event_target_checked(&ev))) />
            <span class=LABEL>{r.label(label)}</span>
        </label>
    }
}

/// Number row. A whole `step` sends an integer, else a float; either is
/// clamped to `min..=max`.
pub fn num_row(r: &Rows, key: &'static str, label: LabelFn, min: f64, max: f64, step: f64) -> impl IntoView + use<> {
    num_row_with(r, key, label, Num { min, max, step, scale: 1.0, int: step.fract() == 0.0 })
}

/// A number field's bounds, in what it shows: the stored value × `scale`
/// (100 for a fraction shown in %, 0.001 for ms shown in s); `int` sends
/// the stored value as an integer.
#[derive(Clone, Copy)]
pub struct Num {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub scale: f64,
    pub int: bool,
}

/// Number row on a stored value shown scaled.
pub fn num_row_with(r: &Rows, key: &'static str, label: LabelFn, n: Num) -> impl IntoView + use<> {
    let (set, values) = (r.set.clone(), r.values);
    let shown = move |v: f64| {
        let v = v * n.scale;
        // Rounded to the field's step, without float noise.
        let digits = (-n.step.log10()).ceil().max(0.0) as usize;
        if n.scale == 1.0 { v.to_string() } else { format!("{v:.digits$}") }
    };
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{r.label(label)}</span>
            <input type="number" inputmode="decimal" class=NUM
                   min=n.min.to_string() max=n.max.to_string() step=n.step.to_string()
                   prop:value=move || values.with(|s| s.get(key).and_then(Value::as_f64).map(shown).unwrap_or_default())
                   on:change=move |ev| {
                       let Ok(v) = event_target_value(&ev).parse::<f64>() else { return };
                       let v = v.clamp(n.min, n.max) / n.scale;
                       set(key, if n.int { Value::from(v.round() as i64) } else { Value::from(v) });
                   } />
        </div>
    }
}

/// Combo row; a current value missing from `options` is kept selectable.
pub fn select_row(r: &Rows, key: &'static str, label: LabelFn, options: &'static [&'static str]) -> impl IntoView + use<> {
    let (set, values) = (r.set.clone(), r.values);
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{r.label(label)}</span>
            <select class=SELECT on:change=move |ev| set(key, Value::String(event_target_value(&ev)))>
                {move || {
                    let cur = values.with(|s| value_str(s, key).unwrap_or_default());
                    let extra = (!cur.is_empty() && !options.contains(&cur.as_str())).then(|| cur.clone());
                    extra.into_iter().chain(options.iter().map(|o| o.to_string())).map(|o| {
                        let (selected, text) = (o == cur, o.clone());
                        view! { <option value=o prop:selected=selected>{text}</option> }
                    }).collect::<Vec<_>>()
                }}
            </select>
        </div>
    }
}

/// A collapsible card.
pub fn section(title: impl Fn() -> &'static str + Send + 'static, open: bool, body: impl IntoView) -> impl IntoView {
    view! {
        <details class="panel group" open=open>
            <summary class="flex items-center gap-2 px-3 min-h-[44px] md:min-h-10 cursor-pointer select-none \
                            list-none [&::-webkit-details-marker]:hidden">
                <span class=format!("{CARD_TITLE} flex-1")>{move || title()}</span>
                <span class="text-text-faint transition-transform group-open:rotate-90">"\u{203a}"</span>
            </summary>
            <div class="px-3 pb-3 flex flex-col">{body}</div>
        </details>
    }
}
