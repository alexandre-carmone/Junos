use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot};
use crate::components::sequence_editor::{SeqFrame, SequenceEditor};
use crate::i18n::{t, Lang};

#[component]
pub fn SchedulerAddJobSection(
    #[prop(into)] lang: RwSignal<Lang>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
    f_target_name: RwSignal<String>,
    f_ra_h: RwSignal<String>,
    f_dec_deg: RwSignal<String>,
    f_min_alt: RwSignal<String>,
    f_min_moon: RwSignal<String>,
    f_pa: RwSignal<String>,
    search_result: RwSignal<Option<String>>,
    form_error: RwSignal<Option<String>>,
    step_track: RwSignal<bool>,
    step_focus: RwSignal<bool>,
    step_align: RwSignal<bool>,
    step_guide: RwSignal<bool>,
    startup_cond: RwSignal<String>,
    startup_at: RwSignal<String>,
    completion_cond: RwSignal<String>,
    completion_count: RwSignal<String>,
    completion_at: RwSignal<String>,
    seq_frames: RwSignal<Vec<SeqFrame>>,
    #[prop(into)] coords_hint: Signal<Option<String>>,
    #[prop(into)] seq_total_hint: Signal<String>,
    on_catalog_search: Arc<dyn Fn() + Send + Sync>,
    on_add_job: Arc<dyn Fn() + Send + Sync>,
    on_clear_form: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    view! {
        <div class="sched-add-section">
            <details class="sched-add-details">
                <summary class="sched-add-summary">{move || tr().sched_add_job_section}</summary>
                <div class="sched-add-body">
                    <div class="sched-field-row">
                        <span class="sched-field-label">{move || tr().sched_target_label}</span>
                        <div class="sched-search-row">
                            <input
                                class="sched-input sched-input-target"
                                placeholder=move || tr().sched_target_placeholder
                                prop:value=move || f_target_name.get()
                                on:input=move |ev| {
                                    f_target_name.set(
                                        ev.target()
                                            .unwrap()
                                            .unchecked_into::<web_sys::HtmlInputElement>()
                                            .value(),
                                    );
                                }
                            />
                            <button
                                class="sched-btn sched-btn-search"
                                on:click=move |_| on_catalog_search()>
                                {move || tr().sched_search_catalog}
                            </button>
                            {move || search_result.get().map(|r| view! {
                                <span class="sched-search-result">{r}</span>
                            })}
                        </div>
                    </div>

                    <div class="sched-field-row">
                        <span class="sched-field-label">{move || tr().sched_ra_label}</span>
                        <input
                            class="sched-input sched-input-ra-dec"
                            placeholder="5.5882"
                            prop:value=move || f_ra_h.get()
                            on:input=move |ev| {
                                f_ra_h.set(
                                    ev.target()
                                        .unwrap()
                                        .unchecked_into::<web_sys::HtmlInputElement>()
                                        .value(),
                                );
                            }
                        />
                        <span class="sched-field-unit">"h"</span>
                        <span class="sched-field-label sched-field-label-offset">{move || tr().sched_dec_label}</span>
                        <input
                            class="sched-input sched-input-ra-dec"
                            placeholder="-5.3911"
                            prop:value=move || f_dec_deg.get()
                            on:input=move |ev| {
                                f_dec_deg.set(
                                    ev.target()
                                        .unwrap()
                                        .unchecked_into::<web_sys::HtmlInputElement>()
                                        .value(),
                                );
                            }
                        />
                        <span class="sched-field-unit">"°"</span>
                    </div>
                    {move || coords_hint.get().map(|h| view! {
                        <div class="sched-coords-hint">{h}</div>
                    })}

                    <div class="sched-field-row">
                        <span class="sched-field-label">{move || tr().sched_min_alt}</span>
                        <input
                            class="sched-input sched-input-small"
                            prop:value=move || f_min_alt.get()
                            on:input=move |ev| {
                                f_min_alt.set(
                                    ev.target()
                                        .unwrap()
                                        .unchecked_into::<web_sys::HtmlInputElement>()
                                        .value(),
                                );
                            }
                        />
                        <span class="sched-field-unit">"°"</span>
                        <span class="sched-field-label sched-field-label-offset">{move || tr().sched_moon_sep}</span>
                        <input
                            class="sched-input sched-input-small"
                            prop:value=move || f_min_moon.get()
                            on:input=move |ev| {
                                f_min_moon.set(
                                    ev.target()
                                        .unwrap()
                                        .unchecked_into::<web_sys::HtmlInputElement>()
                                        .value(),
                                );
                            }
                        />
                        <span class="sched-field-unit">"°"</span>
                        <span class="sched-field-label sched-field-label-offset">{move || tr().sched_pa_label}</span>
                        <input
                            class="sched-input sched-input-small"
                            prop:value=move || f_pa.get()
                            on:input=move |ev| {
                                f_pa.set(
                                    ev.target()
                                        .unwrap()
                                        .unchecked_into::<web_sys::HtmlInputElement>()
                                        .value(),
                                );
                            }
                        />
                        <span class="sched-field-unit">"°"</span>
                    </div>

                    <fieldset class="sched-fieldset">
                        <legend>{move || tr().sched_steps_legend}</legend>
                        <div class="sched-field-row sched-field-row-gap16">
                            <label class="sched-toggle-label">
                                <input type="checkbox"
                                    prop:checked=move || step_track.get()
                                    on:change=move |ev| {
                                        step_track.set(
                                            ev.target()
                                                .unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .checked(),
                                        );
                                    }
                                />
                                {move || tr().sched_step_track}
                            </label>
                            <label class="sched-toggle-label">
                                <input type="checkbox"
                                    prop:checked=move || step_focus.get()
                                    on:change=move |ev| {
                                        step_focus.set(
                                            ev.target()
                                                .unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .checked(),
                                        );
                                    }
                                />
                                {move || tr().sched_step_focus}
                            </label>
                            <label class="sched-toggle-label">
                                <input type="checkbox"
                                    prop:checked=move || step_align.get()
                                    on:change=move |ev| {
                                        step_align.set(
                                            ev.target()
                                                .unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .checked(),
                                        );
                                    }
                                />
                                {move || tr().sched_step_align}
                            </label>
                            <label class="sched-toggle-label">
                                <input type="checkbox"
                                    prop:checked=move || step_guide.get()
                                    on:change=move |ev| {
                                        step_guide.set(
                                            ev.target()
                                                .unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .checked(),
                                        );
                                    }
                                />
                                {move || tr().sched_step_guide}
                            </label>
                        </div>
                    </fieldset>

                    <fieldset class="sched-fieldset">
                        <legend>{move || tr().sched_start_when}</legend>
                        <div class="sched-field-row">
                            <select
                                class="sched-select"
                                on:change=move |ev| {
                                    startup_cond.set(
                                        ev.target()
                                            .unwrap()
                                            .unchecked_into::<web_sys::HtmlSelectElement>()
                                            .value(),
                                    );
                                }>
                                <option value="asap">{move || tr().sched_cond_asap}</option>
                                <option value="at">{move || tr().sched_cond_at_time}</option>
                            </select>
                            {move || (startup_cond.get() == "at").then(|| view! {
                                <input
                                    type="datetime-local"
                                    class="sched-input"
                                    prop:value=move || startup_at.get()
                                    on:input=move |ev| {
                                        startup_at.set(
                                            ev.target()
                                                .unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .value(),
                                        );
                                    }
                                />
                            })}
                        </div>
                    </fieldset>

                    <fieldset class="sched-fieldset">
                        <legend>{move || tr().sched_complete_when}</legend>
                        <div class="sched-field-row">
                            <select
                                class="sched-select"
                                on:change=move |ev| {
                                    completion_cond.set(
                                        ev.target()
                                            .unwrap()
                                            .unchecked_into::<web_sys::HtmlSelectElement>()
                                            .value(),
                                    );
                                }>
                                <option value="sequence">{move || tr().sched_cond_seq}</option>
                                <option value="repeat">{move || tr().sched_cond_repeat}</option>
                                <option value="loop">{move || tr().sched_cond_loop}</option>
                                <option value="at">{move || tr().sched_cond_finish_at}</option>
                            </select>
                            {move || match completion_cond.get().as_str() {
                                "repeat" => view! {
                                    <input
                                        type="number"
                                        class="sched-input sched-input-small"
                                        min="1"
                                        prop:value=move || completion_count.get()
                                        on:input=move |ev| {
                                            completion_count.set(
                                                ev.target()
                                                    .unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>()
                                                    .value(),
                                            );
                                        }
                                    />
                                    <span class="sched-field-unit">{move || tr().sched_times_unit}</span>
                                }
                                    .into_any(),
                                "at" => view! {
                                    <input
                                        type="datetime-local"
                                        class="sched-input"
                                        prop:value=move || completion_at.get()
                                        on:input=move |ev| {
                                            completion_at.set(
                                                ev.target()
                                                    .unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>()
                                                    .value(),
                                            );
                                        }
                                    />
                                }
                                    .into_any(),
                                _ => view! { <span></span> }.into_any(),
                            }}
                        </div>
                    </fieldset>

                    <div class="sched-seq-section">
                        <span class="sched-seq-label">{move || tr().sched_seq_label}</span>
                        <SequenceEditor frames=seq_frames camera=camera filter_wheel=filter_wheel />
                        {move || {
                            let hint = seq_total_hint.get();
                            (!hint.is_empty()).then(|| view! {
                                <div class="sched-seq-total">{hint}</div>
                            })
                        }}
                    </div>

                    {move || form_error.get().map(|e| view! {
                        <div class="sched-form-error">{e}</div>
                    })}
                    <div class="sched-form-btns">
                        <button class="sched-add-btn" on:click=move |_| on_add_job()>
                            {move || tr().sched_add_job_btn}
                        </button>
                        <button class="sched-btn-clear" on:click=move |_| on_clear_form()>
                            {move || tr().sched_clear_btn}
                        </button>
                    </div>
                </div>
            </details>
        </div>
    }
}
