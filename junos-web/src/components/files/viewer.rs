//! Files tab: the viewer, one file over the list. Same layout as Imaging: the
//! frame pinned on phones with the info cards scrolling beneath, frame | cards
//! from `md`, and a pinned footer (Delete · Download · Resolve & Slew). A
//! schedule or sequence (a mosaic import writes them here) shows formatted, as
//! in Files › Planning, with its Load button in place of Resolve & Slew.
//! ‹ ›, the arrow keys or a swipe on the frame step through the folder's files
//! in list order; a tap opens the frame full screen.

use leptos::prelude::*;
use serde_json::Value;
use web_sys::PointerEvent;

use crate::components::form::{CARD, CARD_TITLE, FOOTER};
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;

use super::actions;
use super::api::fetch_meta;
use super::planning::{actions as plan_actions, api::fetch_text, parse::Kind, view as plan_view};
use super::types::FileMeta;
use super::utils::{
    ext_of, format_mtime, format_size, fov_str, is_image_ext, name_of, preview_url, thumb_url, url_encode,
    value_or_dash, DASH, TRASH_ICON,
};
use super::{Item, Shared};

/// A horizontal drag past this many px is a swipe, not a tap.
const SWIPE_PX: f64 = 60.0;

pub(super) fn viewer(s: Shared, files: Memo<Vec<Item>>, send: SendCmd, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    let rel = Memo::new(move |_| s.selected.get().unwrap_or_default());
    let image = move || rel.with(|r| is_image_ext(ext_of(r)));
    // Place in the list, for "3 / 42" and ‹ ›; None once filtered out.
    let pos = Memo::new(move |_| {
        rel.with(|r| files.with(|f| f.iter().position(|(x, _)| x == r).map(|i| (i, f.len()))))
    });

    let plan_kind = move || rel.with(|r| Kind::from_ext(ext_of(r)));

    let meta = RwSignal::new(None::<Result<FileMeta, String>>);
    // A planning file's text, fetched once its mtime is known (`/raw` is
    // cached for a minute; the mtime busts it after a re-save).
    let plan_text = RwSignal::new(None::<Result<String, String>>);
    Effect::new(move |_| {
        let r = rel.get();
        meta.set(None);
        plan_text.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            let m = fetch_meta(&r).await;
            if rel.try_get_untracked().as_ref() != Some(&r) {
                return;
            }
            let mtime = m.as_ref().ok().map(|m| m.mtime);
            meta.set(Some(m));
            if let (Some(mtime), Some(_)) = (mtime, Kind::from_ext(ext_of(&r))) {
                let text = fetch_text(&format!("/api/files/raw?path={}&v={mtime}", url_encode(&r))).await;
                if rel.try_get_untracked().as_ref() == Some(&r) {
                    plan_text.set(Some(text));
                }
            }
        });
    });

    // Swipe: where the pointer went down, and whether it swiped (no tap then).
    let down_x = StoredValue::new(None::<f64>);
    let swiped = StoredValue::new(false);
    let on_down = move |ev: PointerEvent| {
        down_x.set_value(Some(ev.client_x() as f64));
        swiped.set_value(false);
    };
    let on_up = move |ev: PointerEvent| {
        if let Some(x0) = down_x.get_value() {
            let dx = ev.client_x() as f64 - x0;
            if dx.abs() > SWIPE_PX {
                swiped.set_value(true);
                s.step(files, if dx < 0.0 { 1 } else { -1 });
            }
        }
        down_x.set_value(None);
    };
    let on_tap = move |_| {
        if !swiped.get_value() && image() {
            s.zoom(preview_url(&rel.get_untracked()));
        }
    };

    let on_delete = move |_| {
        let r = rel.get_untracked();
        // Then show the next file, else the previous one, else the list.
        let next = files.with_untracked(|f| {
            let i = f.iter().position(|(x, _)| *x == r)?;
            f.get(i + 1).or_else(|| f.get(i.checked_sub(1)?)).map(|(x, _)| x.clone())
        });
        actions::delete(r, s.flash, tr(), move || {
            match next {
                Some(n) => s.selected.set(Some(n)),
                None => s.viewer.set(false),
            }
            s.reload();
        });
    };
    let send_load = send.clone();
    let on_slew = move |_| actions::resolve_and_slew(s.abs(&rel.get_untracked()), &send, s.flash, tr());
    let on_load = move |_| {
        if let Some(kind) = plan_kind() {
            plan_actions::load(kind, s.abs(&rel.get_untracked()), send_load.clone(), s.file_reply, s.flash, tr());
        }
    };

    view! {
        <div class="absolute inset-0 z-[60] bg-bg text-text flex flex-col overflow-hidden">
            // Header: back · name · position · ‹ ›
            <div class="shrink-0 flex items-center gap-1.5 min-h-[48px] px-2 md:pl-3 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <button class="btn-icon shrink-0 text-lg" title=move || tr().files_close_preview
                        on:click=move |_| s.viewer.set(false)>
                    "\u{2190}"
                </button>
                <span class="flex-1 min-w-0 truncate font-semibold text-text-blue-bright"
                      title=move || rel.get()>
                    {move || rel.with(|r| name_of(r).to_string())}
                </span>
                <span class="shrink-0 font-mono text-xs text-text-muted">
                    {move || pos.get().map(|(i, n)| format!("{} / {n}", i + 1))}
                </span>
                <button class="btn-icon shrink-0 text-xl" title=move || tr().files_prev
                        disabled=move || !pos.get().is_some_and(|(i, _)| i > 0)
                        on:click=move |_| s.step(files, -1)>
                    "\u{2039}"
                </button>
                <button class="btn-icon shrink-0 text-xl" title=move || tr().files_next
                        disabled=move || !pos.get().is_some_and(|(i, n)| i + 1 < n)
                        on:click=move |_| s.step(files, 1)>
                    "\u{203A}"
                </button>
            </div>

            <div class="flex-1 min-h-0 flex flex-col md:grid md:grid-cols-[minmax(0,1fr)_360px] \
                        lg:grid-cols-[minmax(0,1fr)_400px] md:grid-rows-[minmax(0,1fr)] md:gap-3 md:p-3 md:pl-4 md:pr-6">
                // Frame. The cached thumbnail shows at once; the full render
                // covers it once loaded (same box, both `object-contain`).
                <div class="relative shrink-0 h-[40dvh] min-h-[180px] overflow-hidden flex items-center justify-center \
                            bg-bg-input-deep border-b border-border-base select-none [touch-action:pan-y] \
                            md:h-auto md:min-h-0 md:border md:rounded-lg"
                     class:cursor-zoom-in=image
                     on:pointerdown=on_down on:pointerup=on_up on:pointercancel=move |_| down_x.set_value(None)
                     on:click=on_tap>
                    {move || {
                        let r = rel.get();
                        if is_image_ext(ext_of(&r)) {
                            view! {
                                <img class="absolute inset-0 w-full h-full object-contain" src=thumb_url(&r)
                                     draggable="false" alt="" />
                                <img class="absolute inset-0 w-full h-full object-contain" src=preview_url(&r)
                                     draggable="false" alt="" />
                            }.into_any()
                        } else {
                            view! {
                                <span class="font-mono text-3xl uppercase text-text-faint">{ext_of(&r).to_string()}</span>
                            }.into_any()
                        }
                    }}
                </div>

                // Info
                <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] flex flex-col gap-3 p-3 md:p-0">
                    {move || match meta.get() {
                        None => view! {
                            <div class="py-8 text-center text-sm text-text-faint">{tr().files_loading}</div>
                        }.into_any(),
                        Some(Err(e)) => view! {
                            <div class="panel p-3 text-sm text-state-err break-words">{format!("{}: {e}", tr().files_error)}</div>
                        }.into_any(),
                        Some(Ok(m)) => info_cards(&m, rel.get_untracked(), s, tr()).into_any(),
                    }}
                    {move || plan_kind().zip(plan_text.get()).map(|(kind, text)| match text {
                        Ok(body) => plan_view::content(kind, body, tr()).into_any(),
                        Err(e) => view! {
                            <div class="panel p-3 text-sm text-state-err break-words">{format!("{}: {e}", tr().files_error)}</div>
                        }.into_any(),
                    })}
                </div>
            </div>

            <div class=format!("{FOOTER} md:pl-4 md:pr-6")>
                <button class="btn-icon btn-danger shrink-0 !w-11 !h-11" title=move || tr().files_delete
                        on:click=on_delete>
                    <span class="inline-block w-5 h-5" inner_html=TRASH_ICON></span>
                </button>
                <a class="btn btn-ghost h-11 px-4 no-underline max-md:flex-1 md:ml-auto"
                   href=move || format!("/api/files/download?path={}", rel.with(|r| url_encode(r)))
                   download="">
                    {move || tr().files_download}
                </a>
                <Show when=image>
                    <button class="btn btn-primary h-11 px-5 font-semibold max-md:flex-1" on:click=on_slew.clone()>
                        {move || tr().files_resolve_slew}
                    </button>
                </Show>
                {move || plan_kind().and_then(|k| plan_actions::load_label(k, tr())).map(|label| {
                    let on_load = on_load.clone();
                    view! {
                        <button class="btn btn-primary h-11 px-5 font-semibold max-md:flex-1"
                                disabled=move || !s.online.get()
                                title=move || if s.online.get() { "" } else { tr().plan_offline }
                                on:click=on_load>
                            {label}
                        </button>
                    }
                })}
            </div>
        </div>
    }
}

/// Label left, value right.
fn kv(label: &'static str, value: String) -> impl IntoView {
    let title = value.clone();
    view! {
        <div class="flex items-baseline justify-between gap-3 text-sm">
            <span class="shrink-0 text-text-muted">{label}</span>
            <span class="min-w-0 truncate text-right font-mono text-text-blue-bright" title=title>{value}</span>
        </div>
    }
}

/// A card of the rows that have a value; nothing when none has.
fn kv_card(title: &'static str, rows: Vec<(&'static str, String)>) -> Option<impl IntoView> {
    let rows: Vec<_> = rows.into_iter().filter(|(_, v)| v != DASH).collect();
    (!rows.is_empty()).then(|| view! {
        <div class=CARD>
            <span class=CARD_TITLE>{title}</span>
            {rows.into_iter().map(|(k, v)| kv(k, v)).collect_view()}
        </div>
    })
}

fn info_cards(m: &FileMeta, rel: String, s: Shared, tr: &'static Translations) -> impl IntoView + use<> {
    let p = m.fits.as_ref().map_or(Value::Null, |f| f.parsed.clone());
    let v = |k: &str| value_or_dash(p.get(k));
    let solved = p["plate_solved"].as_bool().map_or(DASH, |b| if b { tr.yes } else { tr.no }).to_string();
    let header = m.fits.as_ref().map(|f| f.header.clone()).unwrap_or_default();
    let abs = s.abs(&rel);
    let copy = abs.clone();

    view! {
        {kv_card(tr.files_capture_basics, vec![
            (tr.files_exposure, v("exposure")),
            (tr.files_frame_type, v("frame_type")),
            (tr.files_filter, v("filter")),
            (tr.files_gain, v("gain")),
            (tr.files_binning, v("binning")),
            (tr.files_temp, v("ccd_temp")),
        ])}
        {kv_card(tr.files_optical, vec![
            (tr.files_target, v("target")),
            (tr.files_focal, v("focal_length")),
            (tr.files_pixel_size, v("pixel_size")),
        ])}
        {kv_card(tr.files_astrometry, vec![
            (tr.files_ra, v("ra")),
            (tr.files_dec, v("dec")),
            (tr.files_fov, fov_str(p.get("fov_arcmin"))),
            (tr.files_rotation, v("rotation")),
            (tr.files_plate_solved, solved),
        ])}

        <div class=CARD>
            <span class=CARD_TITLE>{tr.files_section_file}</span>
            {kv(tr.files_size, format_size(m.size))}
            {kv(tr.files_mtime, format_mtime(m.mtime, false))}
            <span class="break-all select-all font-mono text-xs text-text-muted">{abs}</span>
            <div class="grid grid-cols-2 gap-2">
                <button class="btn btn-ghost h-11 md:h-9 px-2"
                        on:click=move |_| actions::copy_text(&copy, s.flash, tr.files_path_copied)>
                    {tr.files_copy_path}
                </button>
                <button class="btn btn-ghost h-11 md:h-9 px-2"
                        on:click=move |_| actions::rename(rel.clone(), s.flash, tr, move |new_rel| {
                            s.selected.set(Some(new_rel));
                            s.reload();
                        })>
                    {tr.files_rename}
                </button>
            </div>
        </div>

        {(!header.is_empty()).then(|| view! {
            <details class="panel overflow-hidden">
                <summary class=format!("{CARD_TITLE} cursor-pointer px-3 py-3")>{tr.files_raw_header}</summary>
                <div class="max-h-[50vh] overflow-auto px-3 pb-3 font-mono text-xs">
                    {header.into_iter().map(|r| view! {
                        <div class="grid grid-cols-[4.5rem_minmax(0,1fr)] gap-x-3 py-1 border-b border-border-base">
                            <span class="text-text-blue">{r.key}</span>
                            <span class="break-all text-text-dim">
                                {r.value}
                                {(!r.comment.is_empty()).then(|| view! {
                                    <span class="block italic text-text-faint">{r.comment}</span>
                                })}
                            </span>
                        </div>
                    }).collect_view()}
                </div>
            </details>
        })}
    }
}
