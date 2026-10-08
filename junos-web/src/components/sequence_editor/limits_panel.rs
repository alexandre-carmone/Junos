//! The editor's "Focus & guiding" section: the sequence-wide `SeqLimits`.
//!
//! Collapsed, it's one line listing the enabled limits. It follows Ekos'
//! current values (`capture_get_all_settings`) until the user changes one;
//! "Use Ekos values" goes back to following them.

use leptos::prelude::*;
use serde_json::Value;

use crate::components::form::{CHECK, NUM, ROW};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang};

use super::limits::SeqLimits;

/// Wraps, unlike `form::LABEL`: these labels are sentences.
const TEXT: &str = "min-w-0 text-sm leading-snug text-text-blue";

/// Apply a user edit — from then on the panel stops following Ekos.
fn edit(limits: RwSignal<SeqLimits>, change: impl FnOnce(&mut SeqLimits)) {
    limits.update(|l| {
        change(l);
        l.edited = true;
    });
}

fn checkbox(
    limits: RwSignal<SeqLimits>,
    on: fn(&SeqLimits) -> bool,
    set_on: fn(&mut SeqLimits, bool),
) -> impl IntoView {
    view! {
        <input type="checkbox" class=CHECK
               prop:checked=move || limits.with(on)
               on:change=move |ev| {
                   let v = event_target_checked(&ev);
                   edit(limits, |l| set_on(l, v));
               } />
    }
}

/// A switch alone (refocus after a flip, HFR check).
fn flag_row(
    limits: RwSignal<SeqLimits>,
    label: impl Fn() -> &'static str + Send + Sync + 'static,
    on: fn(&SeqLimits) -> bool,
    set_on: fn(&mut SeqLimits, bool),
) -> impl IntoView {
    view! {
        <label class=format!("{ROW} gap-3 cursor-pointer")>
            {checkbox(limits, on, set_on)}
            <span class=TEXT>{label}</span>
        </label>
    }
}

/// A switch gating a value, like `form::constraint_row`. The value turns red
/// while it's enabled but unusable.
#[allow(clippy::too_many_arguments)]
fn value_row(
    limits: RwSignal<SeqLimits>,
    label: impl Fn() -> &'static str + Send + Sync + 'static,
    on: fn(&SeqLimits) -> bool,
    set_on: fn(&mut SeqLimits, bool),
    value: fn(&SeqLimits) -> String,
    set_value: fn(&mut SeqLimits, String),
    usable: fn(&SeqLimits) -> bool,
    inputmode: &'static str,
    unit: &'static str,
) -> impl IntoView {
    let invalid = move || limits.with(|l| on(l) && !usable(l));
    view! {
        <div class=ROW>
            <label class="flex-1 min-w-0 flex items-center gap-3 cursor-pointer">
                {checkbox(limits, on, set_on)}
                <span class=TEXT>{label}</span>
            </label>
            <input type="text" inputmode=inputmode autocomplete="off"
                   class=move || if invalid() { format!("{NUM} !border-state-err") } else { NUM.to_string() }
                   prop:disabled=move || !limits.with(on)
                   prop:value=move || limits.with(value)
                   on:input=move |ev| {
                       let v = event_target_value(&ev);
                       edit(limits, |l| set_value(l, v));
                   } />
            <span class="w-7 shrink-0 text-sm text-text-muted">{unit}</span>
        </div>
    }
}

#[component]
pub fn LimitsPanel(
    limits: RwSignal<SeqLimits>,
    capture_settings: Signal<Value>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Follow Ekos until the user edits a value; a caller's reset to the
    // default picks Ekos' values up again. Set only on a difference, so the
    // re-run the set triggers stops there.
    let ekos = Memo::new(move |_| capture_settings.with(SeqLimits::from_capture_settings));
    let edited = Memo::new(move |_| limits.with(|l| l.edited));
    Effect::new(move |_| {
        if limits.with(|l| l.edited) { return; }
        if let Some(l) = ekos.get() {
            if limits.with_untracked(|cur| *cur != l) {
                limits.set(l);
            }
        }
    });

    let summary = move || {
        let parts = limits.with(|l| l.summary(tr()));
        if parts.is_empty() { tr().seq_limits_none.to_string() } else { parts.join(" \u{00b7} ") }
    };
    let invalid = move || !limits.with(SeqLimits::is_valid);

    view! {
        <details class=move || if invalid() {
            "group rounded-md border border-state-err"
        } else {
            "group rounded-md border border-border-base"
        }>
            <summary class="list-none cursor-pointer flex items-center gap-sp-2 min-h-11 px-sp-2 select-none \
                            [&::-webkit-details-marker]:hidden">
                <span class="text-text-blue transition-transform group-open:rotate-90">"\u{25B8}"</span>
                <span class="shrink-0 text-xs uppercase tracking-[0.06em] text-text-blue">{move || tr().seq_limits}</span>
                <span class="flex-1 min-w-0 truncate font-mono text-sm text-text-muted">{summary}</span>
            </summary>
            <div class="flex flex-col px-sp-2 pb-sp-2">
                {value_row(limits, move || tr().seq_refocus_every,
                    |l| l.refocus_every, |l, v| l.refocus_every = v,
                    |l| l.refocus_every_min.clone(), |l, v| l.refocus_every_min = v,
                    |l| l.refocus_every_n().is_some(), "numeric", "min")}
                {value_row(limits, move || tr().seq_refocus_temp,
                    |l| l.refocus_temp, |l, v| l.refocus_temp = v,
                    |l| l.refocus_temp_delta.clone(), |l, v| l.refocus_temp_delta = v,
                    |l| l.temp_delta().is_some(), "decimal", "\u{00b0}C")}
                {flag_row(limits, move || tr().seq_refocus_flip,
                    |l| l.refocus_flip, |l, v| l.refocus_flip = v)}
                {flag_row(limits, move || tr().seq_hfr_check,
                    |l| l.hfr_check, |l, v| l.hfr_check = v)}
                {value_row(limits, move || tr().seq_guide_abort,
                    |l| l.guide_abort, |l, v| l.guide_abort = v,
                    |l| l.guide_abort_arcsec.clone(), |l, v| l.guide_abort_arcsec = v,
                    |l| l.guide_abort_value().is_some(), "decimal", "\u{2033}")}
                {value_row(limits, move || tr().seq_guide_start,
                    |l| l.guide_start, |l, v| l.guide_start = v,
                    |l| l.guide_start_arcsec.clone(), |l, v| l.guide_start_arcsec = v,
                    |l| l.guide_start_value().is_some(), "decimal", "\u{2033}")}
                <div class="flex items-start gap-sp-2 pt-sp-1">
                    <span class="flex-1 min-w-0 text-xs leading-snug text-text-muted">{move || tr().seq_limits_hint}</span>
                    <Show when=move || edited.get() && ekos.with(Option::is_some)>
                        <button type="button" class="btn btn--sm btn-ghost shrink-0"
                                on:click=move |_| limits.update(|l| l.edited = false)>
                            {move || tr().seq_limits_from_ekos}
                        </button>
                    </Show>
                </div>
            </div>
        </details>
    }
}
