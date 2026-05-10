//! Files tab — segmented capture browser + promoted LiveStacker panel.
//!
//! File browsing is HTTP-backed and sandboxed by rekos-server's captures root.
//! LiveStacker controls still dispatch raw Ekos Live JSON over the shared WS.

use std::sync::Arc;

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::i18n::{t, Lang};
use crate::ws::{LiveStackerState, SendCmd};
use crate::ws_helpers::send_cmd;
use crate::RevealInFilesCtx;

mod actions;
mod api;
mod browser;
mod livestack;
mod preview;
mod settings;
mod types;
mod utils;

use actions::{copy_to_clipboard, delete_file_action, download_file, rename_file_action};
use api::{fetch_list, fetch_meta, newest_image_in_abs_dir, resolve_abs};
use browser::{filter_button, render_dirs, render_files};
use livestack::render_livestack_workspace;
use preview::render_preview_modal;
use types::{FileMenuState, FileMeta, FilterKind, ListReply, LiveStackTab, SortDir, SortKey};
use utils::{event_select_value, event_value, parent_of, PANEL_BODY, PANEL_CLS, SELECT_CLS, SUMMARY_CLS, INPUT_CLS};

#[component]
pub fn FilesTab(
    livestacker_state: RwSignal<Option<LiveStackerState>>,
    livestacker_settings: RwSignal<Value>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let ls = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    let current_path = RwSignal::new(
        ls.as_ref().and_then(|s| s.get_item("files_path").ok().flatten()).unwrap_or_default()
    );
    let sort_key = RwSignal::new(SortKey::from_storage(ls.as_ref().and_then(|s| s.get_item("files_sort").ok().flatten())));
    let sort_dir = RwSignal::new(SortDir::from_storage(ls.as_ref().and_then(|s| s.get_item("files_sort_dir").ok().flatten())));
    let filter_kind = RwSignal::new(FilterKind::from_storage(ls.as_ref().and_then(|s| s.get_item("files_filter").ok().flatten())));
    let livestack_tab = RwSignal::new(LiveStackTab::from_storage(ls.as_ref().and_then(|s| s.get_item("files_livestack_tab").ok().flatten())));
    let name_filter = RwSignal::new(String::new());

    let listing = RwSignal::new(None::<ListReply>);
    let list_error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let refresh_tick = RwSignal::new(0u32);

    let selected = RwSignal::new(None::<String>);
    let selected_folder = RwSignal::new(None::<String>);
    let preview_open = RwSignal::new(false);
    let selected_meta = RwSignal::new(None::<FileMeta>);
    let meta_error = RwSignal::new(None::<String>);
    let flash = RwSignal::new(None::<String>);
    let file_menu = RwSignal::new(None::<FileMenuState>);

    let latest_stacked = RwSignal::new(None::<String>);
    let latest_stacked_warning = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        let p = current_path.get();
        let k = sort_key.get().storage().to_string();
        let d = sort_dir.get().storage().to_string();
        let f = filter_kind.get().storage().to_string();
        let tab = livestack_tab.get().storage().to_string();
        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = s.set_item("files_path", &p);
            let _ = s.set_item("files_sort", &k);
            let _ = s.set_item("files_sort_dir", &d);
            let _ = s.set_item("files_filter", &f);
            let _ = s.set_item("files_livestack_tab", &tab);
        }
    });

    Effect::new(move |_| {
        let path = current_path.get();
        let _tick = refresh_tick.get();
        loading.set(true);
        list_error.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            match fetch_list(&path).await {
                Ok(reply) => {
                    listing.set(Some(reply));
                    list_error.set(None);
                }
                Err(e) => {
                    listing.set(None);
                    list_error.set(Some(e));
                }
            }
            loading.set(false);
        });
    });

    Effect::new(move |_| {
        let Some(rel) = selected.get() else {
            selected_meta.set(None);
            return;
        };
        meta_error.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            match fetch_meta(&rel).await {
                Ok(m) => {
                    selected_meta.set(Some(m));
                    meta_error.set(None);
                }
                Err(e) => {
                    selected_meta.set(None);
                    meta_error.set(Some(e));
                }
            }
        });
    });

    {
        let send_init = Arc::clone(&send);
        Effect::new(move |prev: Option<()>| {
            if prev.is_none() {
                send_cmd(&send_init, "livestacker_get_all_settings", json!({}));
            }
        });
    }

    if let Some(reveal_ctx) = use_context::<RevealInFilesCtx>() {
        Effect::new(move |_| {
            let Some(abs) = reveal_ctx.0.get() else { return; };
            reveal_ctx.0.set(None);
            wasm_bindgen_futures::spawn_local(async move {
                if abs.is_empty() {
                    current_path.set(String::new());
                    selected.set(None);
                    selected_folder.set(None);
                    preview_open.set(false);
                    return;
                }
                match resolve_abs(&abs).await {
                    Ok(r) if r.in_sandbox => {
                        current_path.set(r.parent.clone());
                        if !r.relative.is_empty() {
                            selected.set(Some(r.relative));
                            preview_open.set(true);
                        }
                    }
                    _ => {
                        current_path.set(String::new());
                        selected.set(None);
                        selected_folder.set(None);
                        preview_open.set(false);
                    }
                }
                refresh_tick.update(|n| *n = n.wrapping_add(1));
            });
        });
    }

    {
        let settings_sig = livestacker_settings;
        let state_sig = livestacker_state;
        Effect::new(move |_| {
            let state = state_sig.get().map(|s| s.state.to_ascii_lowercase()).unwrap_or_default();
            let active = matches!(state.as_str(), "running" | "looping" | "busy" | "active" | "initialized");
            let output = settings_sig.get().get("outputDirectory").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !active || output.is_empty() {
                return;
            }
            latest_stacked_warning.set(None);
            wasm_bindgen_futures::spawn_local(async move {
                match newest_image_in_abs_dir(&output).await {
                    Ok(Some(rel)) => latest_stacked.set(Some(rel)),
                    Ok(None) => {}
                    Err(e) => latest_stacked_warning.set(Some(e)),
                }
            });
        });
    }

    let send_controls = Arc::clone(&send);
    let send_settings = Arc::clone(&send);

    // Mobile tab switcher: 0 = Browser, 1 = LiveStacker
    let mobile_tab = RwSignal::new(0u8);

    view! {
        <div class="absolute inset-0 grid grid-rows-[auto_1fr] overflow-hidden bg-bg text-text font-ui">
            // ── Header bar ──────────────────────────────────────────────────
            <div class="flex min-h-[52px] flex-wrap items-center gap-sp-2 border-b border-border bg-bg-panel-solid py-sp-2 pl-20 pr-sp-4 max-[640px]:pl-sp-3">
                <span class="font-semibold uppercase tracking-[0.08em] text-text-blue">{move || tr().files_title}</span>
                <div class="min-w-0 flex-1 overflow-hidden text-sm text-text-muted">
                    {move || render_breadcrumb(&current_path.get(), tr().files_breadcrumb_root, current_path)}
                </div>
                <Show when=move || loading.get()>
                    <span class="badge badge--info">{move || tr().files_loading}</span>
                </Show>
                <Show when=move || flash.get().is_some()>
                    <span class="badge badge--ok">{move || flash.get().unwrap_or_default()}</span>
                </Show>
            </div>

            // ── Mobile tab switcher bar (≤860px only) ────────────────────
            <div class="hidden max-[860px]:flex border-b border-border bg-bg-elev-1 px-sp-3 py-sp-2 gap-sp-2">
                <button
                    class=move || if mobile_tab.get() == 0 { "btn btn--sm btn--active flex-1" } else { "btn btn--sm btn-ghost flex-1" }
                    on:click=move |_| mobile_tab.set(0)
                >{move || tr().files_section_browser}</button>
                <button
                    class=move || if mobile_tab.get() == 1 { "btn btn--sm btn--active flex-1" } else { "btn btn--sm btn-ghost flex-1" }
                    on:click=move |_| mobile_tab.set(1)
                >{move || tr().livestack_title}</button>
            </div>

            // ── Main content ─────────────────────────────────────────────
            <div class="grid min-h-0 grid-cols-[minmax(320px,1.35fr)_minmax(360px,0.9fr)] gap-sp-4 overflow-hidden p-sp-4 max-[860px]:block max-[860px]:overflow-y-auto max-[860px]:p-sp-3">

                // Left column: File browser
                <section
                    class=move || {
                        // On desktop always visible; on mobile hide when LiveStacker tab active
                        let hidden = mobile_tab.get() == 1;
                        if hidden { "flex min-h-0 flex-col gap-sp-3 overflow-hidden max-[860px]:hidden".to_string() }
                        else { "flex min-h-0 flex-col gap-sp-3 overflow-hidden".to_string() }
                    }
                >
                    <details class=PANEL_CLS open>
                        <summary class=SUMMARY_CLS>
                            <span>{move || tr().files_section_browser}</span>
                            <button class="btn btn--sm btn-ghost" on:click=move |ev| { ev.stop_propagation(); refresh_tick.update(|n| *n = n.wrapping_add(1)); }>
                                {move || tr().files_refresh}
                            </button>
                        </summary>
                        <div class=PANEL_BODY>
                            <div class="grid grid-cols-[1fr_auto_auto] gap-sp-2 max-[860px]:grid-cols-1">
                                <input
                                    class=INPUT_CLS
                                    placeholder=move || tr().files_filter_placeholder
                                    prop:value=move || name_filter.get()
                                    on:input=move |ev| name_filter.set(event_value(&ev))
                                />
                                <select class=SELECT_CLS prop:value=move || sort_key.get().storage().to_string() on:change=move |ev| {
                                    sort_key.set(match event_select_value(&ev).as_str() { "date" => SortKey::Date, "size" => SortKey::Size, _ => SortKey::Name });
                                }>
                                    <option value="name">{move || tr().files_sort_name}</option>
                                    <option value="date">{move || tr().files_sort_date}</option>
                                    <option value="size">{move || tr().files_sort_size}</option>
                                </select>
                                <button class="btn btn--sm btn-ghost" on:click=move |_| sort_dir.update(|d| *d = if *d == SortDir::Asc { SortDir::Desc } else { SortDir::Asc })>
                                    {move || if sort_dir.get() == SortDir::Asc { tr().files_sort_asc } else { tr().files_sort_desc }}
                                </button>
                            </div>
                            <div class="mt-sp-3 flex flex-wrap gap-sp-2">
                                {filter_button(FilterKind::Images, filter_kind, move || tr().files_filter_images)}
                                {filter_button(FilterKind::Fits, filter_kind, move || tr().files_filter_fits)}
                                {filter_button(FilterKind::Jpg, filter_kind, move || tr().files_filter_jpg)}
                                {filter_button(FilterKind::All, filter_kind, move || tr().files_filter_all)}
                            </div>
                        </div>
                    </details>

                    <details class=PANEL_CLS open>
                        <summary class=SUMMARY_CLS><span>{move || tr().livestack_section_directories}</span></summary>
                        <div class="max-h-[190px] overflow-y-auto p-sp-2">
                            <Show when=move || !current_path.with(|p| p.is_empty())>
                                <button class="mb-sp-1 flex w-full items-center gap-sp-2 rounded-md border border-transparent px-sp-2 py-sp-2 text-left text-sm text-text-blue hover:border-border-strong hover:bg-bg-elev-1" on:click=move |_| {
                                    let parent = parent_of(&current_path.get());
                                    current_path.set(parent.clone());
                                    selected.set(None);
                                    selected_folder.set(if parent.is_empty() { None } else { Some(parent) });
                                }>
                                    {move || tr().files_parent}
                                </button>
                            </Show>
                            {move || render_dirs(listing.get(), current_path, selected, selected_folder, selected_folder.get())}
                            <Show when=move || list_error.get().is_some()>
                                <div class="p-sp-2 text-sm text-state-err">{move || format!("{}: {}", tr().files_error, list_error.get().unwrap_or_default())}</div>
                            </Show>
                        </div>
                    </details>

                    <div class="panel min-h-0 flex-1 overflow-y-auto p-sp-3">
                        {move || render_files(
                            listing.get(),
                            current_path.get(),
                            selected,
                            selected.get(),
                            sort_key.get(),
                            sort_dir.get(),
                            filter_kind.get(),
                            name_filter.get(),
                            loading.get(),
                            tr(),
                            preview_open,
                            file_menu,
                        )}
                    </div>
                </section>

                // Right column: LiveStacker
                <section
                    class=move || {
                        // On desktop always visible; on mobile hide when Browser tab active
                        let hidden = mobile_tab.get() == 0;
                        if hidden { "min-h-0 overflow-y-auto max-[860px]:hidden".to_string() }
                        else { "min-h-0 overflow-y-auto".to_string() }
                    }
                >
                    {move || render_livestack_workspace(
                        selected_folder.get(),
                        livestack_tab,
                        livestack_tab.get(),
                        livestacker_state,
                        livestacker_settings,
                        latest_stacked,
                        latest_stacked_warning,
                        current_path,
                        selected,
                        refresh_tick,
                        Arc::clone(&send_controls),
                        Arc::clone(&send_settings),
                        tr(),
                    )}
                </section>
            </div>

            <Show when=move || preview_open.get()>
                {move || render_preview_modal(
                    selected.get(),
                    selected_meta.get(),
                    meta_error.get(),
                    tr(),
                    refresh_tick,
                    selected,
                    flash,
                    preview_open,
                )}
            </Show>

            <Show when=move || file_menu.get().is_some()>
                {move || render_file_menu(file_menu, refresh_tick, selected, flash, tr())}
            </Show>
        </div>
    }
}

fn render_file_menu(
    file_menu: RwSignal<Option<FileMenuState>>,
    refresh_tick: RwSignal<u32>,
    selected: RwSignal<Option<String>>,
    flash: RwSignal<Option<String>>,
    tr: &'static crate::i18n::Translations,
) -> impl IntoView + use<> {
    let state = file_menu.get_untracked().unwrap_or(FileMenuState {
        rel: String::new(),
        anchor_x: 0.0,
        anchor_y: 0.0,
    });
    const MENU_W: f64 = 180.0;
    let left = (state.anchor_x - MENU_W).max(8.0);
    let top = state.anchor_y + 4.0;
    let style = format!("left:{}px;top:{}px;width:{}px;", left, top, MENU_W);
    let close = move || file_menu.set(None);

    let rel_d = state.rel.clone();
    let rel_r = state.rel.clone();
    let rel_x = state.rel.clone();
    let rel_c = state.rel.clone();

    view! {
        <div class="fixed inset-0 z-50" on:click=move |_| close()>
            <div
                class="panel absolute flex flex-col gap-sp-1 p-sp-1 shadow-lg"
                style=style
                on:click=move |ev| ev.stop_propagation()
            >
                <button class="btn btn--sm btn-ghost w-full justify-start" on:click=move |_| {
                    download_file(&rel_d);
                    close();
                }>{tr.files_download}</button>
                <button class="btn btn--sm btn-ghost w-full justify-start" on:click=move |_| {
                    rename_file_action(&rel_r, refresh_tick, selected, tr);
                    close();
                }>{tr.files_rename}</button>
                <button class="btn btn--sm btn-ghost w-full justify-start" on:click=move |_| {
                    delete_file_action(&rel_x, refresh_tick, selected, flash, tr);
                    close();
                }>{tr.files_delete}</button>
                <button class="btn btn--sm btn-ghost w-full justify-start" on:click=move |_| {
                    copy_to_clipboard(&rel_c, flash, tr.files_path_copied);
                    close();
                }>{tr.files_copy_path}</button>
            </div>
        </div>
    }
}

fn render_breadcrumb(path: &str, root_label: &'static str, current_path: RwSignal<String>) -> impl IntoView + use<> {
    let mut acc = String::new();
    let mut chips: Vec<(String, String)> = vec![(String::new(), root_label.to_string())];
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() { acc.push('/'); }
        acc.push_str(seg);
        chips.push((acc.clone(), seg.to_string()));
    }
    let total = chips.len();
    chips.into_iter().enumerate().map(|(i, (target, label))| {
        let is_last = i + 1 == total;
        view! {
            <span class="text-text-faint">{if i == 0 { "" } else { " / " }}</span>
            <button
                class=if is_last { "rounded-sm bg-transparent px-sp-1 py-[2px] text-text-dim" } else { "rounded-sm bg-transparent px-sp-1 py-[2px] text-text-blue hover:bg-bg-elev-1" }
                on:click=move |_| current_path.set(target.clone())
            >{label}</button>
        }
    }).collect_view()
}
