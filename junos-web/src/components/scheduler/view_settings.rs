use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

#[component]
pub fn SchedulerSettingsSection(
    #[prop(into)] lang: RwSignal<Lang>,
    #[prop(into)] send: SendCmd,
    greedy: RwSignal<bool>,
    remember_prog: RwSignal<bool>,
    reschedule_err: RwSignal<bool>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    view! {
        <fieldset class="sched-fieldset">
            <legend>{move || tr().sched_settings_section}</legend>
            <div class="sched-toggle-row">
                <label class="sched-toggle-label">
                    <input
                        type="checkbox"
                        prop:checked=move || greedy.get()
                        on:change={
                            let s = Arc::clone(&send);
                            move |ev| {
                                let checked = ev
                                    .target()
                                    .unwrap()
                                    .unchecked_into::<web_sys::HtmlInputElement>()
                                    .checked();
                                greedy.set(checked);
                                send_cmd(
                                    &s,
                                    "scheduler_set_all_settings",
                                    serde_json::json!({"kcfg_GreedyScheduling": checked}),
                                );
                            }
                        }
                    />
                    {move || tr().sched_greedy}
                </label>
                <label class="sched-toggle-label">
                    <input
                        type="checkbox"
                        prop:checked=move || remember_prog.get()
                        on:change={
                            let s = Arc::clone(&send);
                            move |ev| {
                                let checked = ev
                                    .target()
                                    .unwrap()
                                    .unchecked_into::<web_sys::HtmlInputElement>()
                                    .checked();
                                remember_prog.set(checked);
                                send_cmd(
                                    &s,
                                    "scheduler_set_all_settings",
                                    serde_json::json!({"kcfg_RememberJobProgress": checked}),
                                );
                            }
                        }
                    />
                    {move || tr().sched_remember_progress}
                </label>
                <label class="sched-toggle-label">
                    <input
                        type="checkbox"
                        prop:checked=move || reschedule_err.get()
                        on:change={
                            let s = Arc::clone(&send);
                            move |ev| {
                                let checked = ev
                                    .target()
                                    .unwrap()
                                    .unchecked_into::<web_sys::HtmlInputElement>()
                                    .checked();
                                reschedule_err.set(checked);
                                send_cmd(
                                    &s,
                                    "scheduler_set_all_settings",
                                    serde_json::json!({"errorHandlingRescheduleErrorsCB": checked}),
                                );
                            }
                        }
                    />
                    {move || tr().sched_reschedule_error}
                </label>
            </div>
        </fieldset>
    }
}
