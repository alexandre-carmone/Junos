//! Files tab: the toolbar (path, search, sort, type pills) and the listing —
//! folder rows, then a thumbnail grid (two columns on phones).

use leptos::prelude::*;

use crate::components::form::CHIP;
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};

use super::types::{FilterKind, SortDir, SortKey};
use super::utils::{format_mtime, format_size, is_image_ext, join, parent_of, thumb_url, FOLDER_ICON};
use super::{Item, Shared};

const CRUMB: &str = "shrink-0 h-9 min-h-0 min-w-0 px-1.5 rounded-md bg-transparent border-0";

fn filter_label(k: FilterKind, tr: &'static Translations) -> &'static str {
    match k {
        FilterKind::Images => tr.files_filter_images,
        FilterKind::Fits => tr.files_filter_fits,
        FilterKind::Jpg => tr.files_filter_jpg,
        FilterKind::All => tr.files_filter_all,
    }
}

/// "Captures › Light › M31", each crumb opening its folder.
fn crumbs(path: &str, root: &'static str, target: RwSignal<String>) -> impl IntoView + use<> {
    let mut items = vec![(String::new(), root.to_string())];
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        let rel = join(&items[items.len() - 1].0, seg);
        items.push((rel, seg.to_string()));
    }
    let last = items.len() - 1;
    items.into_iter().enumerate().map(|(i, (rel, label))| view! {
        {(i > 0).then(|| view! { <span class="shrink-0 text-text-faint">"\u{203A}"</span> })}
        <button class=if i == last {
                    format!("{CRUMB} font-semibold text-text")
                } else {
                    format!("{CRUMB} text-text-blue hover:bg-bg-elev-2")
                }
                on:click=move |_| target.set(rel.clone())>
            {label}
        </button>
    }).collect_view()
}

pub(super) fn toolbar(
    s: Shared,
    search: RwSignal<String>,
    sort_key: RwSignal<SortKey>,
    sort_dir: RwSignal<SortDir>,
    filter: RwSignal<FilterKind>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    // Keep the deepest crumb in sight.
    let crumb_box = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        s.path.track();
        if let Some(el) = crumb_box.get() {
            request_animation_frame(move || el.set_scroll_left(el.scroll_width()));
        }
    });
    let sort_option = move |k: SortKey, label: fn(&'static Translations) -> &'static str| view! {
        <option value=k.storage() prop:selected=move || sort_key.get() == k>{move || label(tr())}</option>
    };

    view! {
        <div class="shrink-0 flex flex-col gap-2 px-3 py-2 md:pl-4 md:pr-6 border-b border-border-base">
            <div class="flex items-center gap-1 min-w-0">
                <button class="btn-icon shrink-0" title=move || tr().files_parent
                        disabled=move || s.path.with(String::is_empty)
                        on:click=move |_| s.path.update(|p| *p = parent_of(p))>
                    "\u{2191}"
                </button>
                <div node_ref=crumb_box
                     class="flex-1 min-w-0 flex items-center gap-0.5 overflow-x-auto [scrollbar-width:none] \
                            whitespace-nowrap text-sm">
                    {move || crumbs(&s.path.get(), tr().files_breadcrumb_root, s.path)}
                </div>
            </div>
            <div class="flex flex-col gap-2 md:flex-row md:items-center md:gap-3">
                <div class="flex items-center gap-2 md:flex-1 md:max-w-[520px]">
                    <input type="search" class="input flex-1 min-w-0 h-11 md:h-9"
                           placeholder=move || tr().files_filter_placeholder
                           prop:value=move || search.get()
                           on:input=move |ev| search.set(event_target_value(&ev)) />
                    <select class="input shrink-0 h-11 md:h-9"
                            on:change=move |ev| sort_key.set(SortKey::from_storage(Some(event_target_value(&ev))))>
                        {sort_option(SortKey::Date, |t| t.files_sort_date)}
                        {sort_option(SortKey::Name, |t| t.files_sort_name)}
                        {sort_option(SortKey::Size, |t| t.files_sort_size)}
                    </select>
                    <button class="btn-icon shrink-0 !w-11 !h-11 md:!w-9 md:!h-9"
                            title=move || if sort_dir.get() == SortDir::Asc { tr().files_sort_asc } else { tr().files_sort_desc }
                            on:click=move |_| sort_dir.update(|d| *d = if *d == SortDir::Asc { SortDir::Desc } else { SortDir::Asc })>
                        {move || if sort_dir.get() == SortDir::Asc { "\u{2191}" } else { "\u{2193}" }}
                    </button>
                </div>
                <div class="flex items-center gap-1.5 overflow-x-auto [scrollbar-width:none]">
                    {FilterKind::ALL.map(|k| view! {
                        <button type="button"
                                class=move || if filter.get() == k { format!("{CHIP} shrink-0 btn--active") } else { format!("{CHIP} shrink-0") }
                                aria-pressed=move || (filter.get() == k).to_string()
                                on:click=move |_| filter.set(k)>
                            {move || filter_label(k, tr())}
                        </button>
                    })}
                </div>
            </div>
        </div>
    }
}

pub(super) fn listing(
    s: Shared,
    folders: Memo<Vec<Item>>,
    files: Memo<Vec<Item>>,
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let empty = move || {
        !loading.get() && error.with(Option::is_none) && folders.with(Vec::is_empty) && files.with(Vec::is_empty)
    };
    view! {
        {move || error.get().map(|e| view! {
            <div class="panel mb-3 p-3 text-sm text-state-err break-words">{format!("{}: {e}", tr().files_error)}</div>
        })}
        <Show when=move || folders.with(|f| !f.is_empty())>
            <div class="grid gap-2 sm:grid-cols-2 lg:grid-cols-3 mb-3">
                <For each=move || folders.get() key=|(rel, _)| rel.clone()
                     children=move |item| folder_row(s, item) />
            </div>
        </Show>
        <div class="grid grid-cols-2 gap-2 sm:grid-cols-[repeat(auto-fill,minmax(160px,1fr))]">
            <For each=move || files.get() key=|(rel, e)| (rel.clone(), e.mtime, e.size)
                 children=move |item| file_card(s, item) />
        </div>
        <Show when=empty>
            <div class="py-12 text-center text-sm text-text-faint">{move || tr().files_empty_dir}</div>
        </Show>
    }
}

fn folder_row(s: Shared, (rel, e): Item) -> impl IntoView {
    view! {
        <button class="flex items-center gap-3 h-12 px-3 rounded-lg border border-border-base bg-bg-elev-1 text-left \
                       transition-colors hover:bg-bg-elev-2 hover:border-border-mid"
                on:click=move |_| s.path.set(rel.clone())>
            <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=FOLDER_ICON></span>
            <span class="flex-1 min-w-0 truncate text-sm text-text">{e.name}</span>
            <span class="shrink-0 font-mono text-xs text-text-faint">{format_mtime(e.mtime, true)}</span>
            <span class="shrink-0 text-text-faint">"\u{203A}"</span>
        </button>
    }
}

fn file_card(s: Shared, (rel, e): Item) -> impl IntoView {
    let thumb = is_image_ext(&e.ext).then(|| thumb_url(&rel));
    let info = format!("{} \u{00b7} {}", format_mtime(e.mtime, true), format_size(e.size));
    let open = rel.clone();
    view! {
        <button class=move || format!(
                    "flex flex-col min-w-0 overflow-hidden rounded-lg border bg-bg-elev-1 text-left transition-colors \
                     hover:border-border-mid {}",
                    if s.selected.with(|x| x.as_deref() == Some(rel.as_str())) {
                        "border-accent-cyan ring-1 ring-accent-cyan"
                    } else {
                        "border-border-base"
                    })
                on:click=move |_| {
                    s.selected.set(Some(open.clone()));
                    s.viewer.set(true);
                }>
            <div class="aspect-square bg-bg-input-deep flex items-center justify-center overflow-hidden">
                {match thumb {
                    Some(src) => view! {
                        <img class="w-full h-full object-cover" src=src loading="lazy" decoding="async" alt="" />
                    }.into_any(),
                    None => view! {
                        <span class="font-mono text-lg uppercase text-text-faint">{e.ext.clone()}</span>
                    }.into_any(),
                }}
            </div>
            <div class="min-w-0 flex flex-col px-2 py-1.5">
                <span class="truncate text-xs text-text">{e.name.clone()}</span>
                <span class="truncate font-mono text-xs text-text-faint">{info}</span>
            </div>
        </button>
    }
}
