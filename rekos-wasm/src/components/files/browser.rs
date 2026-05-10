use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::i18n::Translations;

use super::types::{DirEntry, FileMenuState, FilterKind, ListReply, SortDir, SortKey};
use super::utils::{
    format_mtime, format_size, is_fits_ext, is_image_ext, is_jpg_ext, url_encode, FILE_CARD,
    FILE_CARD_ACTIVE, FILE_ROW, FILE_ROW_ACTIVE, SMALL_BTN,
};

fn open_menu_for(rel: String, ev: &web_sys::MouseEvent, file_menu: RwSignal<Option<FileMenuState>>) {
    let (x, y) = ev
        .current_target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .map(|el| {
            let r = el.get_bounding_client_rect();
            (r.right(), r.bottom())
        })
        .unwrap_or((0.0, 0.0));
    file_menu.set(Some(FileMenuState { rel, anchor_x: x, anchor_y: y }));
}

pub(super) fn filter_button(
    label_kind: FilterKind,
    signal: RwSignal<FilterKind>,
    label: impl Fn() -> &'static str + Copy + 'static,
) -> impl IntoView {
    view! {
        <button
            class=move || if signal.get() == label_kind { "btn btn--sm btn--active" } else { SMALL_BTN }
            on:click=move |_| signal.set(label_kind)
        >
            {label()}
        </button>
    }
}

pub(super) fn render_dirs(
    listing: Option<ListReply>,
    current_path: RwSignal<String>,
    selected: RwSignal<Option<String>>,
    selected_folder: RwSignal<Option<String>>,
    selected_folder_value: Option<String>,
) -> impl IntoView {
    let dirs: Vec<DirEntry> = listing
        .map(|r| r.entries.into_iter().filter(|e| e.kind == "dir").collect())
        .unwrap_or_default();
    dirs.into_iter().map(|d| {
        let name = d.name.clone();
        let cur = current_path.get_untracked();
        let rel = if cur.is_empty() { name.clone() } else { format!("{}/{}", cur, name) };
        let active = selected_folder_value.as_deref() == Some(rel.as_str());
        let rel_click = rel.clone();
        view! {
            <button
                class=if active { "mb-sp-1 flex w-full items-center gap-sp-2 rounded-md border border-accent-cyan bg-[color-mix(in_srgb,var(--accent-cyan)_10%,var(--bg-elev-1))] px-sp-2 py-sp-2 text-left text-sm text-text" } else { "mb-sp-1 flex w-full items-center gap-sp-2 rounded-md border border-transparent px-sp-2 py-sp-2 text-left text-sm text-text hover:border-border-strong hover:bg-bg-elev-1" }
                on:click=move |_| {
                current_path.set(rel_click.clone());
                selected_folder.set(Some(rel_click.clone()));
                selected.set(None);
            }>
                <span class="text-text-blue">"DIR"</span>
                <span class="min-w-0 truncate">{d.name}</span>
            </button>
        }
    }).collect_view()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_files(
    listing: Option<ListReply>,
    current_path: String,
    selected: RwSignal<Option<String>>,
    selected_value: Option<String>,
    sort_key: SortKey,
    sort_dir: SortDir,
    filter_kind: FilterKind,
    name_filter: String,
    loading: bool,
    tr: &'static Translations,
    preview_open: RwSignal<bool>,
    file_menu: RwSignal<Option<FileMenuState>>,
) -> impl IntoView {
    let mut files: Vec<DirEntry> = listing
        .map(|r| r.entries.into_iter().filter(|e| e.kind == "file").collect())
        .unwrap_or_default();
    let needle = name_filter.trim().to_ascii_lowercase();
    files.retain(|f| {
        let by_kind = match filter_kind {
            FilterKind::Images => is_image_ext(&f.ext),
            FilterKind::Fits => is_fits_ext(&f.ext),
            FilterKind::Jpg => is_jpg_ext(&f.ext),
            FilterKind::All => true,
        };
        by_kind && (needle.is_empty() || f.name.to_ascii_lowercase().contains(&needle))
    });
    files.sort_by(|a, b| match sort_key {
        SortKey::Name => a
            .name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase()),
        SortKey::Date => a.mtime.cmp(&b.mtime),
        SortKey::Size => a.size.cmp(&b.size),
    });
    if sort_dir == SortDir::Desc {
        files.reverse();
    }

    if files.is_empty() && !loading {
        return view! { <div class="p-sp-5 text-center text-sm text-text-faint">{tr.files_empty_dir}</div> }.into_any();
    }

    let selected_snapshot = selected_value.unwrap_or_default();
    view! {
        <div class="grid grid-cols-[repeat(auto-fill,minmax(130px,1fr))] gap-sp-3">
            {files.into_iter().map(|f| {
                let rel = if current_path.is_empty() { f.name.clone() } else { format!("{}/{}", current_path, f.name) };
                if is_image_ext(&f.ext) {
                    render_file_card(f, rel, selected_snapshot.clone(), selected, preview_open, file_menu, tr).into_any()
                } else {
                    render_file_row(f, rel, selected_snapshot.clone(), selected, preview_open, file_menu, tr).into_any()
                }
            }).collect_view()}
        </div>
    }.into_any()
}

fn render_file_card(
    f: DirEntry,
    rel: String,
    selected_snapshot: String,
    selected: RwSignal<Option<String>>,
    preview_open: RwSignal<bool>,
    file_menu: RwSignal<Option<FileMenuState>>,
    tr: &'static Translations,
) -> impl IntoView {
    let thumb = format!("/api/files/thumb?size=256&path={}", url_encode(&rel));
    let rel_select = rel.clone();
    let rel_menu = rel.clone();
    view! {
        <div class=if rel == selected_snapshot { FILE_CARD_ACTIVE } else { FILE_CARD }>
            <button class="flex flex-1 flex-col p-0 text-left" on:click=move |_| { selected.set(Some(rel_select.clone())); preview_open.set(true); }>
                <img class="aspect-square w-full bg-black object-cover" src=thumb loading="lazy" />
                <span class="truncate px-sp-2 py-sp-2 text-xs text-text-muted">{f.name.clone()}</span>
            </button>
            <div class="absolute left-sp-2 right-sp-2 top-sp-2 flex justify-between gap-sp-2">
                <span class="badge">{format_size(f.size)}</span>
                <button
                    class="btn btn--sm btn-ghost bg-bg-panel-solid"
                    title=tr.files_action_menu
                    on:click=move |ev| { ev.stop_propagation(); open_menu_for(rel_menu.clone(), &ev, file_menu); }
                >"..."</button>
            </div>
        </div>
    }
}

fn render_file_row(
    f: DirEntry,
    rel: String,
    selected_snapshot: String,
    selected: RwSignal<Option<String>>,
    preview_open: RwSignal<bool>,
    file_menu: RwSignal<Option<FileMenuState>>,
    tr: &'static Translations,
) -> impl IntoView {
    let rel_select = rel.clone();
    let rel_menu = rel.clone();
    view! {
        <div class=if rel == selected_snapshot { FILE_ROW_ACTIVE } else { FILE_ROW }>
            <button class="min-w-0 flex-1 text-left" on:click=move |_| { selected.set(Some(rel_select.clone())); preview_open.set(true); }>
                <div class="truncate text-text">{f.name}</div>
                <div class="mt-[2px] flex gap-sp-3 text-xs text-text-faint">
                    <span>{format_size(f.size)}</span>
                    <span>{format_mtime(f.mtime)}</span>
                </div>
            </button>
            <button
                class=SMALL_BTN
                title=tr.files_action_menu
                on:click=move |ev| { ev.stop_propagation(); open_menu_for(rel_menu.clone(), &ev, file_menu); }
            >"..."</button>
        </div>
    }
}
