use std::sync::Arc;

use wasm_bindgen::JsCast;

use leptos::prelude::*;

use crate::i18n::{t, Lang};

#[component]
pub fn SchedulerScriptsSection(
    #[prop(into)] lang: RwSignal<Lang>,
    startup_enabled: RwSignal<bool>,
    pre_startup: RwSignal<String>,
    post_startup: RwSignal<String>,
    shutdown_enabled: RwSignal<bool>,
    pre_shutdown: RwSignal<String>,
    post_shutdown: RwSignal<String>,
    on_apply_scripts: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    view! {
        <div class="sched-add-section sched-add-section--compact">
            <details class="sched-add-details">
                <summary class="sched-add-summary">{move || tr().sched_scripts_section}</summary>
                <div class="sched-add-body">
                    <fieldset class="sched-fieldset">
                        <legend>{move || tr().sched_startup_legend}</legend>
                        <div class="sched-field-row sched-field-row-mb8">
                            <label class="sched-toggle-label">
                                <input
                                    type="checkbox"
                                    prop:checked=move || startup_enabled.get()
                                    on:change=move |ev| {
                                        startup_enabled.set(
                                            ev.target()
                                                .unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .checked(),
                                        );
                                    }
                                />
                                {move || tr().sched_enable_startup}
                            </label>
                        </div>
                        <div class="sched-field-row">
                            <span class="sched-field-label">{move || tr().sched_pre_script}</span>
                            <input
                                class="sched-input sched-input-path"
                                placeholder="/path/to/pre_startup.sh"
                                prop:value=move || pre_startup.get()
                                on:input=move |ev| {
                                    pre_startup.set(
                                        ev.target()
                                            .unwrap()
                                            .unchecked_into::<web_sys::HtmlInputElement>()
                                            .value(),
                                    );
                                }
                            />
                        </div>
                        <div class="sched-field-row sched-field-row-mt6">
                            <span class="sched-field-label">{move || tr().sched_post_script}</span>
                            <input
                                class="sched-input sched-input-path"
                                placeholder="/path/to/post_startup.sh"
                                prop:value=move || post_startup.get()
                                on:input=move |ev| {
                                    post_startup.set(
                                        ev.target()
                                            .unwrap()
                                            .unchecked_into::<web_sys::HtmlInputElement>()
                                            .value(),
                                    );
                                }
                            />
                        </div>
                    </fieldset>

                    <fieldset class="sched-fieldset">
                        <legend>{move || tr().sched_shutdown_legend}</legend>
                        <div class="sched-field-row sched-field-row-mb8">
                            <label class="sched-toggle-label">
                                <input
                                    type="checkbox"
                                    prop:checked=move || shutdown_enabled.get()
                                    on:change=move |ev| {
                                        shutdown_enabled.set(
                                            ev.target()
                                                .unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .checked(),
                                        );
                                    }
                                />
                                {move || tr().sched_enable_shutdown}
                            </label>
                        </div>
                        <div class="sched-field-row">
                            <span class="sched-field-label">{move || tr().sched_pre_script}</span>
                            <input
                                class="sched-input sched-input-path"
                                placeholder="/path/to/pre_shutdown.sh"
                                prop:value=move || pre_shutdown.get()
                                on:input=move |ev| {
                                    pre_shutdown.set(
                                        ev.target()
                                            .unwrap()
                                            .unchecked_into::<web_sys::HtmlInputElement>()
                                            .value(),
                                    );
                                }
                            />
                        </div>
                        <div class="sched-field-row sched-field-row-mt6">
                            <span class="sched-field-label">{move || tr().sched_post_script}</span>
                            <input
                                class="sched-input sched-input-path"
                                placeholder="/path/to/post_shutdown.sh"
                                prop:value=move || post_shutdown.get()
                                on:input=move |ev| {
                                    post_shutdown.set(
                                        ev.target()
                                            .unwrap()
                                            .unchecked_into::<web_sys::HtmlInputElement>()
                                            .value(),
                                    );
                                }
                            />
                        </div>
                    </fieldset>

                    <button class="sched-btn-apply" on:click=move |_| on_apply_scripts()>
                        {move || tr().sched_apply_scripts}
                    </button>
                </div>
            </details>
        </div>
    }
}
