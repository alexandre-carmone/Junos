//! Lock position shift (PHD2's Comet Tracking tool): the lock drifts at a
//! set rate, in ″/h or px/h, along RA / Dec or the camera's X / Y. The
//! switch sends `set_lock_shift_enabled`, the rates, units and axes
//! `set_lock_shift_params` whole; the state is `guider_state.lock_shift`.
//! PHD2 turns multi-star guiding off while it shifts, and stops shifting
//! when the lock reaches the frame's edge (`limit_reached`). Below, the
//! tool's rate training: `comet_train_start` / `comet_train_stop` and
//! PHD2's instructions from `guider_tools.comet`.

use leptos::prelude::*;
use serde_json::json;

use crate::components::form::{CARD, CARD_TITLE, CHECK, CHIP, LABEL, ROW};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang};
use crate::ws::{GuiderLockShift, GuiderStatusData};

use super::protocol::{Reply, Rpc};

const NUM: &str = "input input--sm font-mono w-[104px] shrink-0 text-right max-md:h-9";

pub fn lock_shift_card(
    state: RwSignal<Option<GuiderStatusData>>,
    tools: RwSignal<serde_json::Value>,
    rpc: Rpc,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let ls = Memo::new(move |_| state.with(|s| s.as_ref().map(|s| s.lock_shift.clone()).unwrap_or_default()));
    let reply = RwSignal::new(None::<Reply>);
    let error = move || reply.get().and_then(|r| r.get()).and_then(Result::err).map(|e| e.message);

    // The rates as typed, refreshed from the daemon's.
    let rate: [RwSignal<String>; 2] = std::array::from_fn(|_| RwSignal::new(String::new()));
    Effect::new(move |_| {
        let r = ls.with(|l| l.rate);
        for (i, v) in r.iter().enumerate() {
            rate[i].set(format!("{v}"));
        }
    });
    let send_params = {
        let rpc = rpc.clone();
        move |l: GuiderLockShift| {
            let num = |i: usize| rate[i].with_untracked(|s| s.trim().parse::<f64>().ok()).unwrap_or(l.rate[i]);
            reply.set(Some(rpc.call("set_lock_shift_params", json!({
                "rate": [num(0), num(1)], "units": l.units, "axes": l.axes,
            }))));
        }
    };
    let (s1, s2, s3) = (send_params.clone(), send_params.clone(), send_params.clone());
    let chip = move |on: Signal<bool>, label: Signal<&'static str>, act: Box<dyn Fn() + Send + Sync>| view! {
        <button type="button" class=move || if on.get() { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                on:click=move |_| act()>{move || label.get()}</button>
    };
    let field = move |i: usize, send: Box<dyn Fn() + Send + Sync>| view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>
                {move || {
                    let mount = ls.with(|l| l.axes != "X/Y");
                    match (i, mount) { (0, true) => tr().ra_label, (_, true) => tr().dec_label, (0, false) => "X", _ => "Y" }
                }}
            </span>
            <input type="number" inputmode="decimal" step="any" class=NUM
                   prop:value=move || rate[i].get()
                   on:input=move |ev| rate[i].set(event_target_value(&ev))
                   on:change=move |_| send() />
            <span class="w-12 text-sm text-text-muted">{move || if ls.with(|l| l.units.starts_with("arcsec")) { "\u{2033}/h" } else { "px/h" }}</span>
        </div>
    };
    let with_units = move |units: &'static str| {
        let s = s1.clone();
        Box::new(move || s(GuiderLockShift { units: units.into(), ..ls.get_untracked() })) as Box<dyn Fn() + Send + Sync>
    };
    let with_axes = move |axes: &'static str| {
        let s = s2.clone();
        Box::new(move || s(GuiderLockShift { axes: axes.into(), ..ls.get_untracked() })) as Box<dyn Fn() + Send + Sync>
    };
    let rpc_en = rpc.clone();
    let rpc_comet = rpc.clone();
    let training = move || tools.with(|t| t["comet"]["training"].as_bool().unwrap_or(false));
    view! {
        <div class=CARD>
            <span class=CARD_TITLE>{move || tr().guider_lock_shift_title}</span>
            <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                <input type="checkbox" class=CHECK prop:checked=move || ls.with(|l| l.enabled)
                       on:change=move |ev| reply.set(Some(rpc_en.call("set_lock_shift_enabled", json!({ "enabled": event_target_checked(&ev) })))) />
                <span class=LABEL>{move || tr().guider_lock_shift_enable}</span>
            </label>
            {field(0, { let s = s3.clone(); Box::new(move || s(ls.get_untracked())) })}
            {field(1, { let s = s3.clone(); Box::new(move || s(ls.get_untracked())) })}
            <div class="flex flex-wrap gap-1.5">
                {chip(Signal::derive(move || ls.with(|l| l.units.starts_with("arcsec"))), Signal::derive(|| "\u{2033}/h"), with_units("arcsec/hr"))}
                {chip(Signal::derive(move || !ls.with(|l| l.units.starts_with("arcsec"))), Signal::derive(|| "px/h"), with_units("pixels/hr"))}
                {chip(Signal::derive(move || ls.with(|l| l.axes != "X/Y")), Signal::derive(|| "RA/Dec"), with_axes("RA/Dec"))}
                {chip(Signal::derive(move || ls.with(|l| l.axes == "X/Y")), Signal::derive(|| "X/Y"), with_axes("X/Y"))}
            </div>
            <Show when=move || ls.with(|l| l.limit_reached)>
                <span class="text-xs text-state-warn">{move || tr().guider_lock_shift_limit}</span>
            </Show>
            <span class="text-xs text-text-muted">{move || tr().guider_lock_shift_note}</span>
            <div class=ROW>
                <span class=format!("{LABEL} flex-1")>{move || tr().guider_comet_training}</span>
                <button class=move || if training() { "btn btn-danger h-9 px-4" } else { "btn btn-ghost h-9 px-4" }
                        on:click=move |_| {
                            let m = if training() { "comet_train_stop" } else { "comet_train_start" };
                            reply.set(Some(rpc_comet.call(m, json!({}))));
                        }>
                    {move || if training() { tr().guider_btn_stop } else { tr().guider_ga_start }}
                </button>
            </div>
            <span class="text-xs text-text-muted whitespace-pre-line">
                {move || tools.with(|t| t["comet"]["status"].as_str().unwrap_or("").to_string())}
            </span>
            {move || error().map(|m| view! { <span class="text-xs text-state-err">{m}</span> })}
        </div>
    }
}
