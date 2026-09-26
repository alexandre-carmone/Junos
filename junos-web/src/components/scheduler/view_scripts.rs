//! Scheduler tab: the startup and shutdown procedure slots.
//!
//! Each slot holds the absolute path of a KStars task-queue collection
//! (`.json`, not a shell script — see `queue_model.rs`). The picker lists the
//! collections junos-server manages; "Edit…" opens the queue editor.

use std::sync::Arc;

use wasm_bindgen::JsCast;

use leptos::prelude::*;

use super::queue_api::QueueList;
use super::queue_model::QueueSlot;
use crate::dom::event_target_value;
use crate::i18n::{t, Lang};

const PATH_PLACEHOLDER: &str = "~/.local/share/kstars/taskqueue/collections/….json";

#[component]
pub fn SchedulerScriptsSection(
    #[prop(into)] lang: RwSignal<Lang>,
    startup_enabled: RwSignal<bool>,
    pre_startup: RwSignal<String>,
    post_startup: RwSignal<String>,
    shutdown_enabled: RwSignal<bool>,
    pre_shutdown: RwSignal<String>,
    post_shutdown: RwSignal<String>,
    queue_list: RwSignal<Option<QueueList>>,
    on_apply_scripts: Arc<dyn Fn() + Send + Sync>,
    on_edit_queue: Arc<dyn Fn(QueueSlot) + Send + Sync>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    view! {
        <fieldset class="sched-fieldset">
            <legend>{move || tr().sched_scripts_section}</legend>
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
                <ScriptSlotRow
                    lang=lang
                    queue_slot=QueueSlot::PreStartup
                    path=pre_startup
                    queue_list=queue_list
                    on_edit=Arc::clone(&on_edit_queue)
                />
                <ScriptSlotRow
                    lang=lang
                    queue_slot=QueueSlot::PostStartup
                    path=post_startup
                    queue_list=queue_list
                    on_edit=Arc::clone(&on_edit_queue)
                />
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
                <ScriptSlotRow
                    lang=lang
                    queue_slot=QueueSlot::PreShutdown
                    path=pre_shutdown
                    queue_list=queue_list
                    on_edit=Arc::clone(&on_edit_queue)
                />
                <ScriptSlotRow
                    lang=lang
                    queue_slot=QueueSlot::PostShutdown
                    path=post_shutdown
                    queue_list=queue_list
                    on_edit=Arc::clone(&on_edit_queue)
                />
            </fieldset>

            <button class="sched-btn-apply" on:click=move |_| on_apply_scripts()>
                {move || tr().sched_apply_scripts}
            </button>
        </fieldset>
    }
}

/// One slot: free path field, a picker over the managed collections (applied
/// with the section's "Apply" button, like a typed path), and "Edit…".
#[component]
fn ScriptSlotRow(
    lang: RwSignal<Lang>,
    queue_slot: QueueSlot,
    path: RwSignal<String>,
    queue_list: RwSignal<Option<QueueList>>,
    on_edit: Arc<dyn Fn(QueueSlot) + Send + Sync>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let slot = queue_slot;
    let is_pre = matches!(slot, QueueSlot::PreStartup | QueueSlot::PreShutdown);
    let row_class = if is_pre { "sched-field-row" } else { "sched-field-row sched-field-row-mt6" };

    view! {
        <div class=row_class>
            <span class="sched-field-label">
                {move || if is_pre { tr().sched_pre_script } else { tr().sched_post_script }}
            </span>
            <input
                class="sched-input sched-input-path"
                placeholder=PATH_PLACEHOLDER
                prop:value=move || path.get()
                on:input=move |ev| path.set(event_target_value(&ev))
            />
            <select
                class="sched-select max-w-[240px]"
                on:change=move |ev| {
                    let v = event_target_value(&ev);
                    if !v.is_empty() { path.set(v); }
                }
            >
                <option value="" prop:selected=move || {
                    let p = path.get();
                    queue_list.with(|l| l.as_ref().is_none_or(|l| l.queues.iter().all(|q| q.path != p.trim())))
                }>
                    {move || tr().sched_q_custom_path}
                </option>
                {move || queue_list.get().map(|l| l.queues).unwrap_or_default().into_iter().map(|q| {
                    let label = match &q.title {
                        Some(title) if !title.is_empty() => format!("{title} ({}.json)", q.name),
                        _ => format!("{}.json", q.name),
                    };
                    let value = q.path.clone();
                    view! {
                        <option value=q.path prop:selected=move || path.with(|p| p.trim() == value)>
                            {label}
                        </option>
                    }
                }).collect::<Vec<_>>()}
            </select>
            <button class="sched-btn-icon" on:click=move |_| on_edit(slot)>
                {move || tr().sched_q_edit}
            </button>
        </div>
    }
}
