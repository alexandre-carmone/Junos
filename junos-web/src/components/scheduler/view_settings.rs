//! Scheduler tab: the settings sheet — scheduler options, applied at once —
//! and [`SchedulerSettings`], which also holds the startup / shutdown
//! procedure slots the Startup & shutdown sub-tab (`view_procedures.rs`) edits.

use leptos::prelude::*;
use serde_json::Value;

use super::queue_model::QueueSlot;
use crate::components::form::{check_row, CARD, CARD_TITLE, LABEL};
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

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
}

#[component]
pub fn SchedulerSettingsSheet(
    settings: SchedulerSettings,
    #[prop(into)] send: SendCmd,
    lang: RwSignal<Lang>,
    /// Close the sheet and show the Startup & shutdown sub-tab.
    on_open_procedures: Callback<()>,
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

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().sched_settings_section}</span>
                {check_row(st.greedy, move || tr().sched_greedy, option("kcfg_GreedyScheduling"))}
                {check_row(st.remember_progress, move || tr().sched_remember_progress, option("kcfg_RememberJobProgress"))}
                {check_row(st.reschedule_errors, move || tr().sched_reschedule_error, option("errorHandlingRescheduleErrorsCB"))}
            </div>
            <button type="button" class="panel w-full p-3 flex items-center gap-2 min-h-[44px] text-left cursor-pointer"
                    on:click=move |_| on_open_procedures.run(())>
                <span class=format!("{LABEL} flex-1")>{move || tr().sched_proc_open}</span>
                <span class="text-accent-cyan">"\u{203a}"</span>
            </button>
        </div>
    }
}
