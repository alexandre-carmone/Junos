//! Scheduler tab: the settings sheet — scheduler options, applied at once,
//! and the startup / shutdown procedures, applied with one button.
//!
//! Each procedure slot holds the absolute path of a KStars task-queue
//! collection (`.json`, not a shell script — see `queue_model.rs`). The
//! picker lists the collections junos-server manages; Edit… opens the queue
//! editor.

use leptos::prelude::*;
use serde_json::Value;

use super::queue_api::QueueList;
use super::queue_model::QueueSlot;
use super::view_queue_editor::slot_title;
use crate::components::form::{check_row, CARD, CARD_TITLE, FOOTER, LABEL};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

const PATH_PLACEHOLDER: &str = "~/.local/share/kstars/taskqueue/collections/….json";
const FIELD: &str = "input input--sm w-full min-w-0 max-md:h-9";

/// Scheduler-wide settings, seeded once from `scheduler_get_all_settings`.
#[derive(Clone, Copy)]
pub struct SchedulerSettings {
    pub greedy: RwSignal<bool>,
    pub remember_progress: RwSignal<bool>,
    pub reschedule_errors: RwSignal<bool>,
    pub startup_enabled: RwSignal<bool>,
    pub shutdown_enabled: RwSignal<bool>,
    pub pre_startup: RwSignal<String>,
    pub post_startup: RwSignal<String>,
    pub pre_shutdown: RwSignal<String>,
    pub post_shutdown: RwSignal<String>,
}

impl SchedulerSettings {
    pub fn new() -> Self {
        Self {
            greedy: RwSignal::new(false),
            remember_progress: RwSignal::new(true),
            reschedule_errors: RwSignal::new(false),
            startup_enabled: RwSignal::new(false),
            shutdown_enabled: RwSignal::new(false),
            pre_startup: RwSignal::new(String::new()),
            post_startup: RwSignal::new(String::new()),
            pre_shutdown: RwSignal::new(String::new()),
            post_shutdown: RwSignal::new(String::new()),
        }
    }

    /// Copy the keys KStars sent; missing ones keep their value.
    pub fn seed(&self, s: &Value) {
        let flag = |key: &str, sig: RwSignal<bool>| if let Some(v) = s[key].as_bool() { sig.set(v) };
        let path = |key: &str, sig: RwSignal<String>| if let Some(v) = s[key].as_str() { sig.set(v.to_string()) };
        flag("kcfg_GreedyScheduling", self.greedy);
        flag("kcfg_RememberJobProgress", self.remember_progress);
        flag("errorHandlingRescheduleErrorsCB", self.reschedule_errors);
        flag("schedulerStartupEnabled", self.startup_enabled);
        flag("schedulerShutdownEnabled", self.shutdown_enabled);
        path("schedulerPreStartupScript", self.pre_startup);
        path("schedulerPostStartupScript", self.post_startup);
        path("schedulerPreShutdownScript", self.pre_shutdown);
        path("schedulerPostShutdownScript", self.post_shutdown);
    }

    /// A slot's path field and its procedure's enable toggle.
    pub fn slot(&self, slot: QueueSlot) -> (RwSignal<String>, RwSignal<bool>) {
        match slot {
            QueueSlot::PreStartup   => (self.pre_startup, self.startup_enabled),
            QueueSlot::PostStartup  => (self.post_startup, self.startup_enabled),
            QueueSlot::PreShutdown  => (self.pre_shutdown, self.shutdown_enabled),
            QueueSlot::PostShutdown => (self.post_shutdown, self.shutdown_enabled),
        }
    }

    fn procedures_json(&self) -> Value {
        serde_json::json!({
            "schedulerStartupEnabled":     self.startup_enabled.get_untracked(),
            "schedulerPreStartupScript":   self.pre_startup.get_untracked(),
            "schedulerPostStartupScript":  self.post_startup.get_untracked(),
            "schedulerShutdownEnabled":    self.shutdown_enabled.get_untracked(),
            "schedulerPreShutdownScript":  self.pre_shutdown.get_untracked(),
            "schedulerPostShutdownScript": self.post_shutdown.get_untracked(),
        })
    }
}

#[component]
pub fn SchedulerSettingsSheet(
    settings: SchedulerSettings,
    queue_list: RwSignal<Option<QueueList>>,
    #[prop(into)] send: SendCmd,
    lang: RwSignal<Lang>,
    open: RwSignal<bool>,
    on_edit_queue: Callback<QueueSlot>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let st = settings;

    // An option toggle goes to KStars straight away.
    let option = |key: &'static str| {
        let send = send.clone();
        move |v: bool| {
            let mut m = serde_json::Map::new();
            m.insert(key.into(), v.into());
            send_cmd(&send, "scheduler_set_all_settings", Value::Object(m));
        }
    };
    let send_apply = send.clone();
    let on_apply = move |_| {
        send_cmd(&send_apply, "scheduler_set_all_settings", st.procedures_json());
        open.set(false);
    };
    let slot = move |s: QueueSlot| slot_row(s, st.slot(s).0, queue_list, lang, on_edit_queue);

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().sched_settings_section}</span>
                {check_row(st.greedy, move || tr().sched_greedy, option("kcfg_GreedyScheduling"))}
                {check_row(st.remember_progress, move || tr().sched_remember_progress, option("kcfg_RememberJobProgress"))}
                {check_row(st.reschedule_errors, move || tr().sched_reschedule_error, option("errorHandlingRescheduleErrorsCB"))}
            </div>
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().sched_startup_legend}</span>
                {check_row(st.startup_enabled, move || tr().sched_enable_startup, |_| {})}
                {slot(QueueSlot::PreStartup)}
                {slot(QueueSlot::PostStartup)}
            </div>
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().sched_shutdown_legend}</span>
                {check_row(st.shutdown_enabled, move || tr().sched_enable_shutdown, |_| {})}
                {slot(QueueSlot::PreShutdown)}
                {slot(QueueSlot::PostShutdown)}
            </div>
        </div>
        <div class=FOOTER>
            <button class="btn btn-primary h-11 px-5 ml-auto" on:click=on_apply>
                {move || tr().sched_apply_scripts}
            </button>
        </div>
    }
}

/// One procedure slot: a picker over the managed queues and Edit…; a path
/// field only for a path the picker doesn't know (picking "custom" clears it).
fn slot_row(
    slot: QueueSlot,
    path: RwSignal<String>,
    queue_list: RwSignal<Option<QueueList>>,
    lang: RwSignal<Lang>,
    on_edit: Callback<QueueSlot>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let custom = move || {
        path.with(|p| queue_list.with(|l| l.as_ref().is_none_or(|l| l.queues.iter().all(|q| q.path != p.trim()))))
    };
    view! {
        <div class="flex flex-col gap-1.5 pt-1">
            <div class="flex items-center gap-2">
                <span class=format!("{LABEL} flex-1")>{move || slot_title(tr(), slot)}</span>
                <button class="btn btn--sm btn-ghost shrink-0 max-md:h-9" on:click=move |_| on_edit.run(slot)>
                    {move || tr().sched_q_edit}
                </button>
            </div>
            <select class=FIELD on:change=move |ev| path.set(event_target_value(&ev))>
                <option value="" prop:selected=custom>{move || tr().sched_q_custom_path}</option>
                {move || queue_list.get().map(|l| l.queues).unwrap_or_default().into_iter().map(|q| {
                    let label = match &q.title {
                        Some(title) if !title.is_empty() => format!("{title} ({}.json)", q.name),
                        _ => format!("{}.json", q.name),
                    };
                    let value = q.path.clone();
                    view! {
                        <option value=q.path prop:selected=move || path.with(|p| p.trim() == value)>{label}</option>
                    }
                }).collect::<Vec<_>>()}
            </select>
            <Show when=custom>
                <input class=format!("{FIELD} font-mono") placeholder=PATH_PLACEHOLDER
                       prop:value=move || path.get()
                       on:input=move |ev| path.set(event_target_value(&ev)) />
            </Show>
        </div>
    }
}
