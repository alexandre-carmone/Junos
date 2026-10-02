//! Files tab — browse the captures, view a frame with its FITS header, and
//! drive KStars' Live Stack.
//!
//! Layout (phone-first, like Imaging): a header (Live Stack · refresh), a
//! toolbar (path, search, sort, type pills), then the folder rows and the
//! thumbnail grid, scrolling (`browser.rs`). A tap on a file opens the viewer
//! (`viewer.rs`), a page over the list; Live Stack opens as a `sheet`
//! (`livestack.rs`, `settings.rs`). One `frame_zoom` serves both.
//!
//! Browsing is HTTP (`/api/files/*`, sandboxed to the server's captures root);
//! Resolve & Slew and Live Stack go to KStars over the shared WS.

mod actions;
mod api;
mod browser;
mod livestack;
mod settings;
mod types;
mod utils;
mod viewer;

use leptos::prelude::*;
use serde_json::Value;

use crate::components::form::{sheet, CHIP};
use crate::components::tab_wheel_icons::tab_icon;
use crate::components::zoom::frame_zoom;
use crate::i18n::{t, Lang};
use crate::ws::{LiveStackerState, SendCmd};
use crate::{CaptureDirCtx, RevealInFilesCtx, Tab};

use api::{fetch_list, resolve_abs};
use types::{DirEntry, FilterKind, ListReply, SortDir, SortKey};
use utils::{join, REFRESH_ICON};

/// A listed entry with its sandbox-relative path.
type Item = (String, DirEntry);

/// The signals the tab's parts share.
#[derive(Clone, Copy)]
struct Shared {
    /// Folder shown, relative to the captures root ("" = the root).
    path: RwSignal<String>,
    /// File in the viewer — or the last one viewed, ringed in the grid.
    selected: RwSignal<Option<String>>,
    viewer: RwSignal<bool>,
    refresh: RwSignal<u32>,
    /// Toast text; clears itself.
    flash: RwSignal<Option<String>>,
    /// Full-screen frame (`frame_zoom`).
    zoom_url: RwSignal<Option<String>>,
    zoom_open: RwSignal<bool>,
    /// Absolute captures root on the host (`CaptureDirCtx`).
    root: RwSignal<String>,
}

impl Shared {
    fn reload(self) {
        self.refresh.update(|n| *n = n.wrapping_add(1));
    }

    fn zoom(self, url: String) {
        self.zoom_url.set(Some(url));
        self.zoom_open.set(true);
    }

    /// The host path KStars needs for a sandbox-relative one; "" while the
    /// root is unknown.
    fn abs(self, rel: &str) -> String {
        let root = self.root.get_untracked();
        if root.is_empty() || rel.is_empty() { root } else { join(root.trim_end_matches('/'), rel) }
    }

    /// Show a host path: a folder opens, a file opens in the viewer. Outside
    /// the captures root, the root.
    fn reveal(self, abs: String) {
        wasm_bindgen_futures::spawn_local(async move {
            let r = match resolve_abs(&abs).await {
                Ok(r) if r.in_sandbox && !abs.is_empty() => r,
                _ => {
                    self.path.set(String::new());
                    self.viewer.set(false);
                    return;
                }
            };
            if fetch_list(&r.relative).await.is_ok() {
                self.path.set(r.relative);
                self.viewer.set(false);
            } else {
                self.path.set(r.parent);
                self.selected.set(Some(r.relative));
                self.viewer.set(true);
            }
            self.reload();
        });
    }

    /// Step the viewer `by` files through the list.
    fn step(self, files: Memo<Vec<Item>>, by: isize) {
        let Some(cur) = self.selected.get_untracked() else { return };
        let next = files.with_untracked(|f| {
            let i = f.iter().position(|(rel, _)| *rel == cur)?;
            f.get(i.checked_add_signed(by)?).map(|(rel, _)| rel.clone())
        });
        if next.is_some() {
            self.selected.set(next);
        }
    }
}

fn stored(key: &str) -> Option<String> {
    web_sys::window()?.local_storage().ok()??.get_item(key).ok()?
}

#[component]
pub fn FilesTab(
    livestacker_state: RwSignal<Option<LiveStackerState>>,
    livestacker_settings: RwSignal<Value>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let s = Shared {
        path: RwSignal::new(stored("files_path").unwrap_or_default()),
        selected: RwSignal::new(None),
        viewer: RwSignal::new(false),
        refresh: RwSignal::new(0),
        flash: RwSignal::new(None),
        zoom_url: RwSignal::new(None),
        zoom_open: RwSignal::new(false),
        root: use_context::<CaptureDirCtx>().map_or_else(|| RwSignal::new(String::new()), |c| c.0),
    };
    let sort_key = RwSignal::new(SortKey::from_storage(stored("files_sort")));
    let sort_dir = RwSignal::new(SortDir::from_storage(stored("files_sort_dir")));
    let filter = RwSignal::new(FilterKind::from_storage(stored("files_filter")));
    let search = RwSignal::new(String::new());
    let stack_open = RwSignal::new(false);

    Effect::new(move |_| {
        let pairs = [
            ("files_path", s.path.get()),
            ("files_sort", sort_key.get().storage().to_string()),
            ("files_sort_dir", sort_dir.get().storage().to_string()),
            ("files_filter", filter.get().storage().to_string()),
        ];
        if let Some(ls) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            for (k, v) in pairs {
                let _ = ls.set_item(k, &v);
            }
        }
    });

    // The folder's listing, re-read on navigation and on refresh.
    let listing = RwSignal::new(None::<ListReply>);
    let list_error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    Effect::new(move |_| {
        let path = s.path.get();
        s.refresh.track();
        loading.set(true);
        wasm_bindgen_futures::spawn_local(async move {
            let reply = fetch_list(&path).await;
            // Another folder was opened meanwhile: its own fetch reports.
            if s.path.try_get_untracked().as_ref() != Some(&path) {
                return;
            }
            match reply {
                Ok(r) => {
                    listing.set(Some(r));
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

    // Entries after search, type filter and sort; ties keep the server's
    // name order.
    let sorted = Memo::new(move |_| {
        let needle = search.get().trim().to_lowercase();
        let (key, dir, kind) = (sort_key.get(), sort_dir.get(), filter.get());
        listing.with(|l| {
            let Some(l) = l else { return Vec::new() };
            let mut v: Vec<Item> = l.entries.iter()
                .filter(|e| e.kind == "dir" || kind.accepts(&e.ext))
                .filter(|e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
                .map(|e| (join(&l.path, &e.name), e.clone()))
                .collect();
            v.sort_by(|(_, a), (_, b)| {
                let o = match key {
                    SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortKey::Date => a.mtime.cmp(&b.mtime),
                    SortKey::Size => a.size.cmp(&b.size),
                };
                if dir == SortDir::Desc { o.reverse() } else { o }
            });
            v
        })
    });
    let of_kind = move |kind: &'static str| {
        Memo::new(move |_| sorted.with(|v| v.iter().filter(|(_, e)| e.kind == kind).cloned().collect::<Vec<_>>()))
    };
    let (folders, files) = (of_kind("dir"), of_kind("file"));

    if let Some(reveal) = use_context::<RevealInFilesCtx>() {
        Effect::new(move |_| {
            if let Some(abs) = reveal.0.get() {
                reveal.0.set(None);
                s.reveal(abs);
            }
        });
    }

    // The toast fades after a moment.
    Effect::new(move |_| {
        let Some(msg) = s.flash.get() else { return };
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(2500).await;
            if s.flash.try_get_untracked().flatten() == Some(msg) {
                s.flash.set(None);
            }
        });
    });

    // Escape closes the top layer; the arrows step the viewer.
    let keys = window_event_listener(leptos::ev::keydown, move |e| {
        let zoomed = s.zoom_open.get_untracked();
        match e.key().as_str() {
            "Escape" if zoomed => s.zoom_open.set(false),
            "Escape" if stack_open.get_untracked() => stack_open.set(false),
            "Escape" => s.viewer.set(false),
            "ArrowLeft" if s.viewer.get_untracked() && !zoomed => s.step(files, -1),
            "ArrowRight" if s.viewer.get_untracked() && !zoomed => s.step(files, 1),
            _ => {}
        }
    });
    on_cleanup(move || keys.remove());

    let stack_state = Memo::new(move |_| livestacker_state.with(|o| o.as_ref().map(|l| l.state.clone()).unwrap_or_default()));
    let send_viewer = StoredValue::new(send.clone());
    let send_stack = StoredValue::new(send);

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Files)></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{move || tr().files_title}</span>
                <button type="button" class=format!("{CHIP} gap-2 ml-auto shrink-0")
                        on:click=move |_| stack_open.set(true)>
                    <span class=move || format!("w-2 h-2 rounded-full {}", livestack::dot(&stack_state.get()))></span>
                    {move || tr().livestack_title}
                </button>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().files_refresh
                        on:click=move |_| s.reload()>
                    <span class=move || if loading.get() { "inline-block w-5 h-5 animate-spin" } else { "inline-block w-5 h-5" }
                          inner_html=REFRESH_ICON></span>
                </button>
            </div>

            {browser::toolbar(s, search, sort_key, sort_dir, filter, lang)}

            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:pl-4 md:pr-6 \
                        pb-[max(0.75rem,env(safe-area-inset-bottom))]">
                {browser::listing(s, folders, files, list_error, loading, lang)}
            </div>

            <Show when=move || s.viewer.get() && s.selected.with(Option::is_some)>
                {viewer::viewer(s, files, send_viewer.get_value(), lang)}
            </Show>
            <Show when=move || stack_open.get()>
                {sheet(move || tr().livestack_title, move || stack_open.set(false), view! {
                    <livestack::LiveStack s=s open=stack_open state=livestacker_state
                                          settings=livestacker_settings send=send_stack.get_value() />
                })}
            </Show>
            {frame_zoom(s.zoom_url.into(), s.zoom_open, lang)}

            {move || s.flash.get().map(|msg| view! {
                <div class="absolute left-1/2 -translate-x-1/2 z-[90] bottom-[max(5rem,calc(env(safe-area-inset-bottom)+4.5rem))] \
                            max-w-[min(90%,420px)] panel px-4 py-2 text-sm text-center text-text shadow-lg pointer-events-none">
                    {msg}
                </div>
            })}
        </div>
    }
}
