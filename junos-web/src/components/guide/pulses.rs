//! Manual guide pulses (PHD2's Manual Guide dialog): a N / S / E / W pad —
//! Up / Down / Left / Right steps for an AO — where each tap sends one
//! `guide_pulse`, with the duration (or step count) chips and the Mount /
//! AO choice. PHD2 refuses a pulse while calibrating or guiding, so the pad
//! is off then; any refusal shows under it.

use leptos::prelude::*;
use serde_json::json;

use crate::components::form::{CARD, CARD_TITLE, CHIP};
use crate::i18n::{t, Lang};
use crate::ws::GuiderStatusData;

use super::protocol::{Reply, Rpc, CALIBRATING, GUIDING, LOST_LOCK};

const MS: [i64; 4] = [100, 250, 500, 1000];
const STEPS: [i64; 4] = [1, 2, 5, 10];

pub fn pulses_card(state: RwSignal<Option<GuiderStatusData>>, rpc: Rpc, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    let ao = RwSignal::new(false);
    let size = RwSignal::new(1_usize);
    let reply = RwSignal::new(None::<Reply>);
    let has_ao = Memo::new(move |_| state.with(|s| s.as_ref().and_then(|s| s.equipment.ao.as_ref()).is_some_and(|a| a.connected)));
    let busy = Memo::new(move |_| {
        state.with(|s| matches!(s.as_ref().and_then(|s| s.app_state.as_deref()), Some(CALIBRATING | GUIDING | LOST_LOCK)))
    });
    let error = move || reply.get().and_then(|r| r.get()).and_then(Result::err).map(|e| e.message);

    let pulse = move |dir: &'static str| {
        let on_ao = ao.get_untracked() && has_ao.get_untracked();
        let amount = if on_ao { STEPS[size.get_untracked()] } else { MS[size.get_untracked()] };
        let which = if on_ao { "ao" } else { "mount" };
        reply.set(Some(rpc.call("guide_pulse", json!({ "amount": amount, "direction": dir, "which": which }))));
    };
    // (mount direction, AO direction, grid cell)
    let pad: [(&str, &str, &str); 4] = [
        ("North", "Up", "col-start-2 row-start-1"),
        ("West", "Left", "col-start-1 row-start-2"),
        ("East", "Right", "col-start-3 row-start-2"),
        ("South", "Down", "col-start-2 row-start-3"),
    ];
    let chip = move |on: Signal<bool>, label: Signal<String>, set: Box<dyn Fn() + Send + Sync>| view! {
        <button type="button" class=move || if on.get() { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                on:click=move |_| set()>{move || label.get()}</button>
    };
    let on_ao = Memo::new(move |_| ao.get() && has_ao.get());
    view! {
        <div class=CARD>
            <span class=CARD_TITLE>{move || tr().guider_pulses_title}</span>
            <Show when=move || has_ao.get()>
                <div class="flex gap-1.5">
                    {chip(Signal::derive(move || !ao.get()), Signal::derive(move || tr().guider_role_mount.to_string()), Box::new(move || ao.set(false)))}
                    {chip(ao.into(), Signal::derive(move || tr().guider_role_ao.to_string()), Box::new(move || ao.set(true)))}
                </div>
            </Show>
            <div class="flex flex-wrap gap-1.5">
                {(0..4).map(|i| chip(
                    Signal::derive(move || size.get() == i),
                    Signal::derive(move || if on_ao.get() { format!("{} {}", STEPS[i], tr().guider_steps) } else { format!("{} ms", MS[i]) }),
                    Box::new(move || size.set(i)),
                )).collect_view()}
            </div>
            <div class="grid grid-cols-3 grid-rows-3 gap-2 w-[216px] mx-auto">
                {pad.map(|(m, a, cell)| {
                    let pulse = pulse.clone();
                    view! {
                        <button class=format!("btn btn-ghost h-14 font-semibold {cell}") disabled=move || busy.get()
                                title=move || if busy.get() { tr().guider_pulses_busy } else { "" }
                                on:click=move |_| pulse(if on_ao.get_untracked() { a } else { m })>
                            {move || if on_ao.get() { a } else { &m[..1] }}
                        </button>
                    }
                })}
            </div>
            {move || error().map(|m| view! { <span class="text-xs text-state-err">{m}</span> })}
        </div>
    }
}
