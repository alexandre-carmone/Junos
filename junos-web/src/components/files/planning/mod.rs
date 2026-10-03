//! Files › Planning — the Scheduler's files, in a sheet over the captures
//! browser: saved schedules (`.esl`), each job's sequence (`.esq`), and the
//! startup / shutdown task queues (`.json`) with their scripts (`.sh`).
//!
//! The list groups them by kind, newest first; a tap opens one formatted
//! (`view.rs`) with a pinned footer — Delete, Rename (schedules only: the
//! others are named by path elsewhere), Download, and Load in Scheduler /
//! Imaging (`actions.rs`). Files come from `/api/planning/*` (`api.rs`), read
//! on the server host — where KStars writes them, same user assumed.

pub(crate) mod actions;
pub(crate) mod api;
pub(crate) mod parse;
pub(crate) mod view;

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE, FOOTER};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;

use super::actions::copy_text;
use super::utils::{format_mtime, format_size, TRASH_ICON};
use super::Shared;
use api::{PlanGroup, PlanList};
use parse::Kind;

/// The file open in the sheet.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PlanFile {
    pub kind: Kind,
    pub name: String,
    /// Its folder on the host, from the listing.
    pub dir: String,
    pub size: u64,
    pub mtime: u64,
}

impl PlanFile {
    /// The host path KStars needs; "" when the server knows no folder.
    fn abs(&self) -> String {
        if self.dir.is_empty() { String::new() } else { format!("{}/{}", self.dir.trim_end_matches('/'), self.name) }
    }
}

fn group_title(kind: Kind, tr: &'static Translations) -> &'static str {
    match kind {
        Kind::Schedules => tr.plan_group_schedules,
        Kind::Sequences => tr.plan_group_sequences,
        Kind::Queues => tr.plan_group_queues,
        Kind::Scripts => tr.plan_group_scripts,
    }
}

fn empty_hint(kind: Kind, tr: &'static Translations) -> &'static str {
    match kind {
        Kind::Schedules => tr.plan_empty_schedules,
        Kind::Sequences => tr.plan_empty_sequences,
        Kind::Queues => tr.plan_empty_queues,
        Kind::Scripts => tr.plan_empty_scripts,
    }
}

fn confirm_delete(kind: Kind, tr: &'static Translations) -> &'static str {
    match kind {
        Kind::Schedules => tr.plan_confirm_delete_schedule,
        Kind::Sequences => tr.plan_confirm_delete_sequence,
        Kind::Queues => tr.plan_confirm_delete_queue,
        Kind::Scripts => tr.plan_confirm_delete_script,
    }
}

/// The sheet's body: the grouped list, or the open file.
pub(super) fn planning(s: Shared, file: RwSignal<Option<PlanFile>>, send: SendCmd) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let list = RwSignal::new(None::<Result<PlanList, String>>);
    let refresh = RwSignal::new(0u32);
    let search = RwSignal::new(String::new());
    Effect::new(move |_| {
        refresh.track();
        wasm_bindgen_futures::spawn_local(async move {
            let reply = api::fetch_list().await;
            list.try_set(Some(reply));
        });
    });
    let reload = move || refresh.update(|n| *n = n.wrapping_add(1));
    let send = StoredValue::new(send);

    view! {
        {move || match file.get() {
            None => file_list(list, search, file, lang).into_any(),
            Some(f) => detail(f, s, file, reload, send.get_value(), lang).into_any(),
        }}
    }
}

fn file_list(
    list: RwSignal<Option<Result<PlanList, String>>>,
    search: RwSignal<String>,
    file: RwSignal<Option<PlanFile>>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] flex flex-col gap-3 p-3 \
                    pb-[max(0.75rem,env(safe-area-inset-bottom))]">
            <input type="search" class="input w-full" placeholder=move || tr().files_filter_placeholder
                   prop:value=move || search.get()
                   on:input=move |ev| search.set(event_target_value(&ev)) />
            {move || {
                let needle = search.get().trim().to_lowercase();
                match list.get() {
                    None => view! {
                        <div class="py-8 text-center text-sm text-text-faint">{tr().files_loading}</div>
                    }.into_any(),
                    Some(Err(e)) => view! {
                        <div class="panel p-3 text-sm text-state-err break-words">{format!("{}: {e}", tr().files_error)}</div>
                    }.into_any(),
                    Some(Ok(l)) => Kind::ALL
                        .into_iter()
                        .map(|k| section(k, l.group(k).cloned(), &needle, file, tr()))
                        .collect_view()
                        .into_any(),
                }
            }}
        </div>
    }
}

fn section(
    kind: Kind,
    group: Option<PlanGroup>,
    needle: &str,
    file: RwSignal<Option<PlanFile>>,
    tr: &'static Translations,
) -> impl IntoView + use<> {
    let PlanGroup { dir, mut entries, .. } = group.unwrap_or(PlanGroup {
        kind: kind.key().to_string(),
        dir: String::new(),
        entries: Vec::new(),
    });
    let total = entries.len();
    entries.retain(|e| needle.is_empty() || e.name.to_lowercase().contains(needle));
    entries.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    let hint = if total == 0 {
        Some(empty_hint(kind, tr))
    } else if entries.is_empty() {
        Some(tr.plan_no_match)
    } else {
        None
    };
    let rows = entries
        .into_iter()
        .map(|e| {
            let open = PlanFile { kind, name: e.name.clone(), dir: dir.clone(), size: e.size, mtime: e.mtime };
            view! {
                <button class="flex items-center gap-3 h-12 px-3 rounded-lg border border-border-base bg-bg-elev-1 \
                               text-left transition-colors hover:bg-bg-elev-2 hover:border-border-mid"
                        on:click=move |_| file.set(Some(open.clone()))>
                    <span class="flex-1 min-w-0 truncate text-sm text-text">{e.name}</span>
                    <span class="shrink-0 font-mono text-xs text-text-faint">{format_mtime(e.mtime, true)}</span>
                    <span class="shrink-0 text-text-faint">"\u{203A}"</span>
                </button>
            }
        })
        .collect_view();

    view! {
        <div class=CARD>
            <div class="flex items-baseline gap-2 min-w-0">
                <span class=format!("{CARD_TITLE} shrink-0")>{group_title(kind, tr)}</span>
                <span class="shrink-0 font-mono text-xs text-text-faint">{total.to_string()}</span>
                <span class="ml-auto min-w-0 truncate font-mono text-xs text-text-faint" title=dir.clone()>{dir.clone()}</span>
            </div>
            {rows}
            {hint.map(|h| view! { <span class="text-sm text-text-faint">{h}</span> })}
        </div>
    }
}

fn detail(
    f: PlanFile,
    s: Shared,
    file: RwSignal<Option<PlanFile>>,
    reload: impl Fn() + Copy + Send + Sync + 'static,
    send: SendCmd,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let kind = f.kind;
    let abs = f.abs();

    let text = RwSignal::new(None::<Result<String, String>>);
    {
        let url = api::raw_url(kind, &f.name);
        wasm_bindgen_futures::spawn_local(async move {
            let reply = api::fetch_text(&url).await;
            text.try_set(Some(reply));
        });
    }

    let back = move || {
        file.set(None);
        reload();
    };
    let name = f.name.clone();
    let on_delete = move |_| {
        let Some(win) = web_sys::window() else { return };
        if !win.confirm_with_message(confirm_delete(kind, tr())).unwrap_or(false) {
            return;
        }
        let (name, tr) = (name.clone(), tr());
        wasm_bindgen_futures::spawn_local(async move {
            match api::delete(kind, &name).await {
                Ok(()) => back(),
                Err(e) => s.flash.set(Some(format!("{}: {e}", tr.files_error))),
            }
        });
    };
    let renamed = f.clone();
    let on_rename = move |_| {
        let Some(win) = web_sys::window() else { return };
        let old = renamed.name.clone();
        let stem = old.rsplit_once('.').map_or(old.as_str(), |(stem, _)| stem);
        let Some(typed) = win.prompt_with_message_and_default(tr().files_rename_prompt, stem).ok().flatten() else {
            return;
        };
        let typed = typed.trim().to_string();
        if typed.is_empty() || typed == stem || typed == old {
            return;
        }
        let (current, tr) = (renamed.clone(), tr());
        wasm_bindgen_futures::spawn_local(async move {
            match api::rename(kind, &current.name, &typed).await {
                Ok(name) => file.set(Some(PlanFile { name, ..current })),
                Err(e) => s.flash.set(Some(format!("{}: {e}", tr.files_error))),
            }
        });
    };
    let load_abs = abs.clone();
    let on_load = move |_| {
        actions::load(kind, load_abs.clone(), send.clone(), s.file_reply, s.flash, tr());
    };
    let copy = abs.clone();
    let download = api::download_url(kind, &f.name);
    let online = s.online;

    view! {
        <div class="shrink-0 flex items-center gap-1.5 px-2 py-1 border-b border-border-base">
            <button class="btn-icon shrink-0 text-lg" title=move || tr().files_close_preview on:click=move |_| back()>
                "\u{2190}"
            </button>
            <span class="flex-1 min-w-0 truncate font-semibold text-text" title=f.name.clone()>{f.name.clone()}</span>
            <span class="shrink-0 font-mono text-xs text-text-faint">{move || group_title(kind, tr())}</span>
        </div>

        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] flex flex-col gap-3 p-3">
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().files_section_file}</span>
                {move || view::kv(tr().files_mtime, format_mtime(f.mtime, false))}
                {move || view::kv(tr().files_size, format_size(f.size))}
                <span class="break-all select-all font-mono text-xs text-text-muted">{abs.clone()}</span>
                <div class=if kind == Kind::Schedules { "grid grid-cols-2 gap-2" } else { "grid gap-2" }>
                    <button class="btn btn-ghost h-11 md:h-9 px-2"
                            on:click=move |_| copy_text(&copy, s.flash, tr().files_path_copied)>
                        {move || tr().files_copy_path}
                    </button>
                    {(kind == Kind::Schedules).then(|| view! {
                        <button class="btn btn-ghost h-11 md:h-9 px-2" on:click=on_rename>
                            {move || tr().files_rename}
                        </button>
                    })}
                </div>
            </div>
            {move || match text.get() {
                None => view! {
                    <div class="py-8 text-center text-sm text-text-faint">{tr().files_loading}</div>
                }.into_any(),
                Some(Err(e)) => view! {
                    <div class="panel p-3 text-sm text-state-err break-words">{format!("{}: {e}", tr().files_error)}</div>
                }.into_any(),
                Some(Ok(body)) => view::content(kind, body, tr()).into_any(),
            }}
        </div>

        <div class=FOOTER>
            <button class="btn-icon btn-danger shrink-0 !w-11 !h-11" title=move || tr().files_delete on:click=on_delete>
                <span class="inline-block w-5 h-5" inner_html=TRASH_ICON></span>
            </button>
            <a class="btn btn-ghost h-11 px-4 no-underline max-md:flex-1 md:ml-auto" href=download download="">
                {move || tr().files_download}
            </a>
            {actions::load_label(kind, tr()).map(|_| view! {
                <button class="btn btn-primary h-11 px-5 font-semibold max-md:flex-1"
                        disabled=move || !online.get()
                        title=move || if online.get() { "" } else { tr().plan_offline }
                        on:click=on_load>
                    {move || actions::load_label(kind, tr()).unwrap_or_default()}
                </button>
            })}
        </div>
    }
}
