//! Scheduler: the Startup & shutdown sub-tab.
//!
//! The night as a timeline of KStars' four procedure slots — before Ekos
//! starts, once devices are connected, before and after Ekos stops — each
//! showing the queue assigned to it, under the startup / shutdown switches.
//! Every change goes to KStars at once (`scheduler_set_all_settings`). A slot
//! opens the queue editor (`view_queue_editor.rs`): over the timeline on
//! phones, beside it from `md`.

use std::collections::HashMap;
use std::sync::Arc;

use leptos::prelude::*;
use serde_json::Value;

use super::queue_api::{self, QueueEntry, QueueList};
use super::queue_model::QueueSlot;
use super::view_queue_editor::SchedulerQueueEditor;
use super::view_settings::SchedulerSettings;
use crate::components::form::{check_row, CARD_TITLE};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};
use crate::ws::{DeviceInfo, IndiProperty, SendCmd};
use crate::ws_helpers::send_cmd;

const FIELD: &str = "input input--sm w-full min-w-0 max-md:h-9";
const PATH_PLACEHOLDER: &str = "~/.local/share/kstars/taskqueue/collections/….json";
/// Assign-select value that reveals the path field (not a valid path).
const CUSTOM: &str = "\u{0}custom";
/// The timeline's rail, behind the numbered dots and milestone diamonds.
const RAIL: &str = "absolute left-[15px] top-0 bottom-0 w-px bg-border-base";

fn stage_title(tr: &'static Translations, slot: QueueSlot) -> &'static str {
    match slot {
        QueueSlot::PreStartup   => tr.sched_proc_pre_startup,
        QueueSlot::PostStartup  => tr.sched_proc_post_startup,
        QueueSlot::PreShutdown  => tr.sched_proc_pre_shutdown,
        QueueSlot::PostShutdown => tr.sched_proc_post_shutdown,
    }
}

fn set_setting(send: &SendCmd, key: &str, value: Value) {
    let mut m = serde_json::Map::new();
    m.insert(key.to_string(), value);
    send_cmd(send, "scheduler_set_all_settings", Value::Object(m));
}

#[component]
pub fn SchedulerProcedures(
    lang: RwSignal<Lang>,
    send: SendCmd,
    settings: SchedulerSettings,
    queue_list: RwSignal<Option<QueueList>>,
    devices: RwSignal<Vec<DeviceInfo>>,
    indi_properties: RwSignal<HashMap<String, Vec<IndiProperty>>>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let editing = RwSignal::new(Option::<QueueSlot>::None);

    // Fresh each time the sub-tab opens; the editor re-lists on its own.
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(list) = queue_api::fetch_list().await {
            queue_list.set(Some(list));
        }
    });

    let phase = |legend: fn(&'static Translations) -> &'static str,
                 enable: fn(&'static Translations) -> &'static str,
                 on: RwSignal<bool>,
                 key: &'static str| {
        let send = Arc::clone(&send);
        view! {
            <li class="flex flex-col pt-2 first:pt-0">
                <span class=CARD_TITLE>{move || legend(tr())}</span>
                {check_row(on, move || enable(tr()), move |v| set_setting(&send, key, v.into()))}
            </li>
        }
    };
    let card = |slot: QueueSlot, number: u8| {
        let (path, enabled) = settings.slot(slot);
        slot_card(slot, number, path, enabled, queue_list, editing, lang, Arc::clone(&send))
    };

    let send_editor = Arc::clone(&send);
    let on_close: Arc<dyn Fn() + Send + Sync> = Arc::new(move || editing.set(None));

    view! {
        <div class="flex-1 min-h-0 flex flex-col md:grid md:grid-cols-[340px_minmax(0,1fr)] \
                    lg:grid-cols-[380px_minmax(0,1fr)] md:grid-rows-[minmax(0,1fr)]">
            // Timeline — hidden on phones while a queue is open.
            <div class=move || format!(
                "min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:pl-4 md:border-r md:border-border-base {}",
                if editing.get().is_some() { "max-md:hidden" } else { "max-md:flex-1" },
            )>
                <p class="m-0 mb-3 text-xs text-text-muted">{move || tr().sched_proc_intro}</p>
                <ol class="m-0 p-0 list-none flex flex-col gap-2">
                    {phase(|tr| tr.sched_startup_legend, |tr| tr.sched_enable_startup, settings.startup_enabled, "schedulerStartupEnabled")}
                    {card(QueueSlot::PreStartup, 1)}
                    {milestone(move || tr().sched_proc_ekos_start)}
                    {card(QueueSlot::PostStartup, 2)}
                    {milestone(move || tr().sched_proc_jobs)}
                    {phase(|tr| tr.sched_shutdown_legend, |tr| tr.sched_enable_shutdown, settings.shutdown_enabled, "schedulerShutdownEnabled")}
                    {card(QueueSlot::PreShutdown, 3)}
                    {milestone(move || tr().sched_proc_ekos_stop)}
                    {card(QueueSlot::PostShutdown, 4)}
                </ol>
            </div>

            // Editor, or a hint on md+ while none is open.
            <div class=move || format!(
                "min-h-0 flex flex-col {}",
                if editing.get().is_some() { "max-md:flex-1" } else { "max-md:hidden" },
            )>
                {move || match editing.get() {
                    Some(slot) => {
                        let (path, enabled) = settings.slot(slot);
                        view! {
                            <SchedulerQueueEditor lang=lang send=Arc::clone(&send_editor) queue_slot=slot
                                                  list=queue_list path=path enabled=enabled devices=devices
                                                  indi_properties=indi_properties on_close=Arc::clone(&on_close) />
                        }.into_any()
                    }
                    None => view! {
                        <div class="flex-1 grid place-items-center p-6 text-sm text-text-faint text-center">
                            {move || tr().sched_proc_pick}
                        </div>
                    }.into_any(),
                }}
            </div>
        </div>
    }
}

/// What happens between two slots (Ekos starting, jobs running, …).
fn milestone(label: impl Fn() -> &'static str + Send + 'static) -> impl IntoView {
    view! {
        <li class="relative pl-10 py-1" aria-hidden="true">
            <span class=RAIL></span>
            <span class="absolute left-[11px] top-1/2 -mt-[4.5px] w-[9px] h-[9px] rotate-45 bg-accent-cyan-dim"></span>
            <span class="text-xs text-text-faint">{move || label()}</span>
        </li>
    }
}

/// One slot: what it runs (tap to edit) and which queue it runs.
#[allow(clippy::too_many_arguments)]
fn slot_card(
    slot: QueueSlot,
    number: u8,
    path: RwSignal<String>,
    enabled: RwSignal<bool>,
    queue_list: RwSignal<Option<QueueList>>,
    editing: RwSignal<Option<QueueSlot>>,
    lang: RwSignal<Lang>,
    send: SendCmd,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let entry = move || -> Option<QueueEntry> {
        let p = path.get();
        queue_list.with(|l| l.as_ref()?.queues.iter().find(|q| q.path == p.trim()).cloned())
    };
    // "Custom path" picked, or a path the list doesn't know.
    let custom_picked = RwSignal::new(false);
    let custom = move || custom_picked.get() || (path.with(|p| !p.trim().is_empty()) && entry().is_none());
    let active = move || editing.get() == Some(slot);

    let summary = move || match entry() {
        Some(q) => {
            let steps = q.tasks.map(|n| format!(" · {n} {}", tr().sched_proc_steps)).unwrap_or_default();
            let title = q.title.filter(|t| !t.is_empty()).unwrap_or_else(|| q.name.clone());
            view! {
                <span class="text-sm text-text truncate">{title}</span>
                <span class="text-xs text-text-muted font-mono truncate">{format!("{}.json{steps}", q.name)}</span>
            }.into_any()
        }
        None if path.with(|p| p.trim().is_empty()) => view! {
            <span class="text-sm text-text-faint">{move || tr().sched_proc_none}</span>
        }.into_any(),
        None => view! {
            <span class="text-sm text-text">{move || tr().sched_proc_custom}</span>
            <span class="text-xs text-text-muted font-mono break-all">{move || path.get()}</span>
        }.into_any(),
    };

    let send_pick = Arc::clone(&send);
    let on_pick = move |ev: leptos::ev::Event| {
        let value = event_target_value(&ev);
        if value == CUSTOM {
            custom_picked.set(true);
            return;
        }
        custom_picked.set(false);
        path.set(value.clone());
        set_setting(&send_pick, slot.setting_key(), value.into());
    };
    let on_path = move |ev: leptos::ev::Event| {
        let value = event_target_value(&ev).trim().to_string();
        path.set(value.clone());
        set_setting(&send, slot.setting_key(), value.into());
    };

    view! {
        <li class="relative pl-10">
            <span class=RAIL></span>
            <span class=move || format!(
                "absolute left-1 top-3 w-[23px] h-[23px] rounded-full grid place-items-center text-xs font-mono \
                 border bg-bg {}",
                if active() { "border-accent-cyan text-accent-cyan" } else { "border-border-strong text-text-muted" },
            )>{number.to_string()}</span>
            <div class=move || format!(
                "panel p-3 flex flex-col gap-2 {} {}",
                if active() { "border-accent-cyan" } else { "" },
                if enabled.get() { "" } else { "opacity-60" },
            )>
                <button type="button" class="w-full p-0 border-0 bg-transparent text-left flex items-center gap-2 \
                                             min-h-[44px] md:min-h-9 cursor-pointer"
                        aria-current=move || active().then_some("true")
                        on:click=move |_| editing.set(Some(slot))>
                    <span class="flex-1 min-w-0 flex flex-col gap-0.5">
                        <span class="text-sm font-semibold text-text-blue">{move || stage_title(tr(), slot)}</span>
                        {summary}
                    </span>
                    <span class="shrink-0 text-sm text-accent-cyan">{move || tr().sched_q_edit}</span>
                </button>
                {(!slot.allows_devices()).then(|| view! {
                    <span class="self-start inline-flex items-center h-6 px-2 rounded-full border border-border-base \
                                 text-xs text-text-dim">
                        {move || tr().sched_proc_no_devices}
                    </span>
                })}
                <select class=FIELD aria-label=move || tr().sched_q_name on:change=on_pick>
                    <option value="" prop:selected=move || !custom() && path.with(|p| p.trim().is_empty())>
                        {move || tr().sched_q_none}
                    </option>
                    {move || queue_list.get().map(|l| l.queues).unwrap_or_default().into_iter().map(|q| {
                        let label = match &q.title {
                            Some(title) if !title.is_empty() => format!("{title} ({}.json)", q.name),
                            _ => format!("{}.json", q.name),
                        };
                        let value = q.path.clone();
                        view! {
                            <option value=q.path prop:selected=move || !custom() && path.with(|p| p.trim() == value)>
                                {label}
                            </option>
                        }
                    }).collect_view()}
                    <option value=CUSTOM prop:selected=custom>{move || tr().sched_q_custom_path}</option>
                </select>
                <Show when=custom>
                    <input class=format!("{FIELD} font-mono") placeholder=PATH_PLACEHOLDER
                           prop:value=move || path.get()
                           on:change=on_path.clone() />
                </Show>
            </div>
        </li>
    }
}
