//! Scheduler: Save schedule — KStars writes the whole job list, with the
//! startup / shutdown procedures, to an `.esl` in the planning folder
//! (`~/.junos-schedules`), which Files › Planning lists and loads back.
//!
//! `scheduler_save_file {filepath}` (message.cpp:1205) answers with the saved
//! file's text — but `Scheduler::save` (scheduler.cpp:2304) skips the write
//! when the list hasn't changed since it was last saved or loaded, and then
//! answers with whatever file already sits at that path, or not at all. So a
//! save counts only when the file's mtime moved.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::files::planning::{api as plan_api, parse::Kind};
use crate::components::form::{setting_row, CARD, FOOTER};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang};
use crate::ws::{FileReply, SendCmd};
use crate::ws_helpers::{file_command, send_cmd};

use super::labels::sanitize_name;

pub const SAVE_ICON: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 5a1 1 0 0 1 1-1h11l4 4v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1z"/><path d="M8 4v5h7V4"/><path d="M7 20v-6h10v6"/></svg>"##;

/// KStars answers a save at once; past this it wrote nothing.
const REPLY_MS: u32 = 5000;

async fn schedule_mtime(file: &str) -> Option<u64> {
    let list = plan_api::fetch_list().await.ok()?;
    list.group(Kind::Schedules)?.entries.iter().find(|e| e.name == file).map(|e| e.mtime)
}

#[component]
pub fn SaveScheduleSheet(
    jobs: Memo<Vec<Value>>,
    home_dir: Signal<String>,
    online: Signal<bool>,
    file_reply: RwSignal<Option<FileReply>>,
    #[prop(into)] send: SendCmd,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let first = jobs.with_untracked(|j| j.first().and_then(|j| j["name"].as_str()).map(sanitize_name));
    let name = RwSignal::new(first.filter(|n| !n.is_empty()).unwrap_or_else(|| "schedule".to_string()));
    let busy = RwSignal::new(false);
    let status = RwSignal::new(None::<Result<String, &'static str>>);

    // The schedules folder junos-server lists; KStars' home until it answers.
    let listed_dir = RwSignal::new(String::new());
    wasm_bindgen_futures::spawn_local(async move {
        if let Some(dir) = plan_api::fetch_list().await.ok().and_then(|l| Some(l.group(Kind::Schedules)?.dir.clone())) {
            listed_dir.try_set(dir);
        }
    });
    let folder = move || {
        let dir = listed_dir.get();
        if !dir.is_empty() {
            return dir;
        }
        let home = home_dir.get();
        if home.is_empty() { String::new() } else { format!("{}/.junos-schedules", home.trim_end_matches('/')) }
    };
    let file_name = move || format!("{}.esl", sanitize_name(name.get().trim()));
    let path = move || {
        let dir = folder();
        if dir.is_empty() { String::new() } else { format!("{}/{}", dir.trim_end_matches('/'), file_name()) }
    };
    let can_save = move || {
        !busy.get() && online.get() && !name.get().trim().is_empty() && !folder().is_empty() && jobs.with(|j| !j.is_empty())
    };

    let on_save = move |_| {
        if !can_save() {
            return;
        }
        let (dir, file, path, tr, send) = (folder(), file_name(), path(), tr(), send.clone());
        busy.set(true);
        status.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            let before = schedule_mtime(&file).await;
            let overwrite_ok = before.is_none()
                || web_sys::window().is_some_and(|w| w.confirm_with_message(tr.plan_confirm_overwrite).unwrap_or(false));
            if overwrite_ok {
                // KStars' QFile doesn't create folders.
                send_cmd(&send, "file_directory_operation", json!({ "operation": "create", "path": dir }));
                let reply = file_command(&send, file_reply, "scheduler_save_file", json!({ "filepath": path }), REPLY_MS).await;
                let after = if reply.is_some() { schedule_mtime(&file).await } else { None };
                let written = after.is_some() && after != before;
                status.try_set(Some(if written { Ok(format!("{} {path}", tr.plan_saved)) } else { Err(tr.plan_nothing_written) }));
            }
            busy.try_set(false);
        });
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            <div class=CARD>
                {setting_row(move || tr().plan_save_name, view! {
                    <input type="text" class="input input--sm w-[220px] shrink-0 max-md:h-9"
                           prop:value=move || name.get()
                           on:input=move |ev| name.set(event_target_value(&ev)) />
                })}
                <span class="break-all font-mono text-xs text-text-muted">{path}</span>
                <p class="m-0 text-xs leading-snug text-text-faint">{move || tr().plan_save_hint}</p>
            </div>
        </div>
        <div class=FOOTER>
            {move || {
                let (class, text) = match status.get() {
                    Some(Ok(msg)) => ("text-state-ok", msg),
                    Some(Err(msg)) => ("text-state-err", msg.to_string()),
                    None if folder().is_empty() => ("text-state-err", tr().plan_no_home.to_string()),
                    None => ("", String::new()),
                };
                view! { <div class=format!("flex-1 min-w-0 break-words text-sm leading-snug {class}")>{text}</div> }
            }}
            <button class="btn btn-primary h-11 px-5 shrink-0 font-semibold" disabled=move || !can_save()
                    on:click=on_save>
                {move || if busy.get() { tr().plan_saving } else { tr().plan_save }}
            </button>
        </div>
    }
}
