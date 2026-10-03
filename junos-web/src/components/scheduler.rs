//! Ekos Scheduler tab.
//!
//! Layout (phone-first, like Focus / Mosaic): a header (status · settings),
//! the job cards and the live log — one scrolling column on phones, jobs |
//! log from `md` — and a pinned footer with Add job and Start / Stop. Add job,
//! Settings and the startup/shutdown queue editor open as `form::sheet`s:
//! bottom sheets on phones, centered panels on md+.
//!
//! Inbound:  `new_scheduler_state`, `scheduler_get_jobs`,
//!           `scheduler_get_all_settings`
//! Outbound: `scheduler_start_job` (a toggle: KStars stops only a RUNNING
//!           scheduler, else starts it), `scheduler_remove_jobs`,
//!           `scheduler_set_all_settings` + `scheduler_add_jobs`,
//!           `scheduler_save_sequence_file`
//! HTTP:     `/api/taskqueue/*` (startup/shutdown queue editor)

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

mod altitude;
mod labels;
mod mapping;
mod queue_api;
mod queue_model;
mod view_add_job;
mod view_jobs;
mod view_log;
mod view_queue_editor;
mod view_settings;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot, SchedulerSnapshot, SiteSnapshot};
use crate::components::form::{sheet, FOOTER};
use crate::components::tab_wheel_icons::tab_icon;
use crate::i18n::{Lang, t};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;
use crate::{SchedulerPrefillCtx, Tab};
use labels::scheduler_status_label;
use queue_api::QueueList;
use queue_model::QueueSlot;
use view_add_job::{AddJobForm, AddJobSheet};
use view_jobs::SchedulerJobs;
use view_log::SchedulerLog;
use view_queue_editor::SchedulerQueueEditor;
use view_settings::{SchedulerSettings, SchedulerSettingsSheet};

/// KStars' `SchedulerState` (ekos.h): RUNNING, and the states a toggle
/// shouldn't interrupt (STARTUP, SHUTDOWN, LOADING).
const RUNNING: i64 = 2;
fn is_transitional(status: i64) -> bool {
    matches!(status, 1 | 4 | 6)
}

#[component]
pub fn SchedulerTab(
    #[prop(into)] scheduler: Signal<SchedulerSnapshot>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Split the snapshot so a log line doesn't rebuild the job cards.
    let status = Memo::new(move |_| scheduler.with(|s| s.status));
    let jobs = Memo::new(move |_| scheduler.with(|s| s.jobs.clone()));
    let log = Memo::new(move |_| scheduler.with(|s| s.log.clone()));
    let home_dir = Signal::derive(move || scheduler.with(|s| s.home_dir.clone()));
    // KStars' log is newest first.
    let latest = move || log.with(|l| l.lines().find(|x| !x.trim().is_empty()).unwrap_or("").to_string());

    // ── Sheets ──────────────────────────────────────────────────────────────
    let add_open      = RwSignal::new(false);
    let settings_open = RwSignal::new(false);
    // Startup/shutdown queue editor, stacked over the settings sheet.
    let queue_editor  = RwSignal::new(Option::<QueueSlot>::None);

    // Escape closes the top sheet — the queue editor first, so it doesn't
    // also dismiss the settings underneath.
    {
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            move |e: web_sys::KeyboardEvent| {
                if e.key() != "Escape" { return; }
                if queue_editor.get_untracked().is_some() {
                    queue_editor.set(None);
                } else {
                    add_open.set(false);
                    settings_open.set(false);
                }
            },
        );
        if let Some(win) = web_sys::window() {
            let _ = win.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        }
        cb.forget();
    }

    // ── Add-job form; the Sky's "Add to Scheduler" fills it and opens it ────
    let form = AddJobForm::new();
    let prefill_ctx = use_context::<SchedulerPrefillCtx>();
    Effect::new(move |_| {
        let Some(pctx) = prefill_ctx else { return };
        let Some((name, ra_deg, dec_deg)) = pctx.0.get() else { return };
        form.set_target(name, ra_deg, dec_deg);
        add_open.set(true);
        pctx.0.set(None);  // consume
    });

    // ── Settings, seeded once from KStars ───────────────────────────────────
    let settings = SchedulerSettings::new();
    let seeded = RwSignal::new(false);
    Effect::new(move |_| {
        if seeded.get_untracked() { return; }
        scheduler.with(|s| {
            if s.settings.is_object() {
                settings.seed(&s.settings);
                seeded.set(true);
            }
        });
    });

    // Collections junos-server manages (`/api/taskqueue/list`), refreshed each
    // time the settings sheet opens; the editor re-lists on its own too.
    let queue_list = RwSignal::new(Option::<QueueList>::None);
    Effect::new(move |_| {
        if !settings_open.get() { return; }
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(list) = queue_api::fetch_list().await {
                queue_list.set(Some(list));
            }
        });
    });
    let on_edit_queue = Callback::new(move |slot| queue_editor.set(Some(slot)));
    let on_close_queue: Arc<dyn Fn() + Send + Sync> = Arc::new(move || queue_editor.set(None));

    let send_toggle = Arc::clone(&send);
    let on_toggle = move |_| send_cmd(&send_toggle, "scheduler_start_job", serde_json::json!({}));

    let send_add = Arc::clone(&send);
    let send_settings = Arc::clone(&send);
    let send_queue = Arc::clone(&send);

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Scheduler)></span>
                <span class="min-w-0 truncate font-semibold text-text-blue-bright">{move || tr().tab_scheduler}</span>
                <span class=move || format!("{} ml-auto shrink-0", scheduler_status_label(tr(), status.get()).1)>
                    {move || scheduler_status_label(tr(), status.get()).0}
                </span>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().sched_settings_btn
                        on:click=move |_| settings_open.set(true)>
                    <span class="inline-block w-5 h-5" inner_html=tab_icon(Tab::Profiles)></span>
                </button>
            </div>

            // Body — one scrolling column on phones; jobs | log on md+, each
            // scrolling on its own.
            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] flex flex-col gap-3 p-3 \
                        md:pl-4 md:pr-6 md:overflow-hidden md:grid md:grid-cols-[minmax(0,1fr)_320px] \
                        lg:grid-cols-[minmax(0,1fr)_400px] md:grid-rows-[minmax(0,1fr)]">
                <SchedulerJobs jobs=jobs site=site send=Arc::clone(&send) lang=lang />
                <SchedulerLog log=log lang=lang />
            </div>

            // Footer: latest log line (md+), Add job, Start / Stop.
            <div class=format!("{FOOTER} md:pl-4 md:pr-6")>
                <span class="max-md:hidden flex-1 min-w-0 truncate font-mono text-xs text-text-muted" title=latest>
                    {latest}
                </span>
                <button class="btn btn-ghost h-11 px-4 max-md:flex-1" on:click=move |_| add_open.set(true)>
                    {move || format!("+ {}", tr().sched_add_job_btn)}
                </button>
                <button
                    class=move || if status.get() == RUNNING {
                        "btn btn-danger h-11 px-5 font-semibold max-md:flex-1"
                    } else {
                        "btn btn-primary h-11 px-5 font-semibold max-md:flex-1"
                    }
                    disabled=move || is_transitional(status.get())
                    on:click=on_toggle>
                    {move || if status.get() == RUNNING { tr().sched_btn_stop } else { tr().sched_btn_start }}
                </button>
            </div>

            <Show when=move || add_open.get()>
                {sheet(move || tr().sched_add_job_btn, move || add_open.set(false), view! {
                    <AddJobSheet form=form site=site camera=camera filter_wheel=filter_wheel home_dir=home_dir
                                 send=Arc::clone(&send_add) lang=lang open=add_open />
                })}
            </Show>
            <Show when=move || settings_open.get()>
                {sheet(move || tr().sched_settings_btn, move || settings_open.set(false), view! {
                    <SchedulerSettingsSheet settings=settings queue_list=queue_list send=Arc::clone(&send_settings)
                                            lang=lang open=settings_open on_edit_queue=on_edit_queue />
                })}
            </Show>
            {move || queue_editor.get().map(|slot| {
                let (path, enabled) = settings.slot(slot);
                view! {
                    <SchedulerQueueEditor lang=lang send=Arc::clone(&send_queue) queue_slot=slot list=queue_list
                                          path=path enabled=enabled on_close=Arc::clone(&on_close_queue) />
                }
            })}
        </div>
    }
}
