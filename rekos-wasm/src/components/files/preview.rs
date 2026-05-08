use leptos::prelude::*;
use serde_json::Value;

use crate::i18n::Translations;

use super::actions::{copy_to_clipboard, delete_file_action, download_file};
use super::types::{FileMeta, FitsRow};
use super::utils::{format_mtime, format_size, fov_str, kv, url_encode, value_or_dash, SMALL_BTN};

pub(super) fn render_preview_modal(
    sel: Option<String>,
    meta: Option<FileMeta>,
    err: Option<String>,
    tr: &'static Translations,
    refresh_tick: RwSignal<u32>,
    selected: RwSignal<Option<String>>,
    flash: RwSignal<Option<String>>,
    preview_open: RwSignal<bool>,
) -> impl IntoView {
    let Some(rel) = sel else {
        preview_open.set(false);
        return view! { <div></div> }.into_any();
    };
    let preview_url = format!("/api/files/raw?as=preview&path={}", url_encode(&rel));
    // Desktop button clones
    let rel_download = rel.clone();
    let rel_delete = rel.clone();
    let rel_copy = rel.clone();
    // Mobile bottom-bar clones
    let rel_download_mob = rel.clone();
    let rel_delete_mob = rel.clone();
    let rel_copy_mob = rel.clone();
    let meta_view = match (err, meta) {
        (Some(e), _) => view! { <div class="p-sp-4 text-sm text-state-err">{format!("{}: {}", tr.files_error, e)}</div> }.into_any(),
        (_, None) => view! { <div class="p-sp-5 text-center text-sm text-text-faint">{tr.files_loading}</div> }.into_any(),
        (_, Some(m)) => render_meta_blocks(&m, tr).into_any(),
    };

    view! {
        // On desktop (≥640px): grid-rows-[auto_1fr_auto] — header | image | metadata
        // On mobile (<640px):  grid-rows-[auto_1fr_auto_auto] — slim-header | image | metadata | action-bar
        <div class="fixed inset-0 z-[70] grid grid-rows-[auto_1fr_auto] bg-[rgba(0,0,0,0.86)] text-text font-ui max-[639px]:grid-rows-[auto_1fr_auto_auto]">

            // ── Header: file path + desktop actions ──────────────────────
            <div class="flex flex-wrap items-center gap-sp-2 border-b border-border bg-bg-panel-solid px-sp-4 py-sp-2">
                <div class="min-w-0 flex-1">
                    <div class="text-sm font-semibold uppercase tracking-[0.06em] text-text-blue">{tr.files_section_preview}</div>
                    <div class="truncate text-xs text-text-faint">{rel.clone()}</div>
                </div>
                // Desktop-only inline action buttons
                <div class="flex items-center gap-sp-2 max-[639px]:hidden">
                    <button class=SMALL_BTN on:click=move |_| download_file(&rel_download)>{tr.files_download}</button>
                    <button class=SMALL_BTN on:click=move |_| copy_to_clipboard(&rel_copy, flash, tr.files_path_copied)>{tr.files_copy_path}</button>
                    <button class="btn btn--sm btn-danger" on:click=move |_| delete_file_action(&rel_delete, refresh_tick, selected, flash, tr)>{tr.files_delete}</button>
                </div>
                <button class="btn btn--sm btn-ghost text-lg" title=tr.files_close_preview on:click=move |_| preview_open.set(false)>"×"</button>
            </div>

            // ── Image viewer ─────────────────────────────────────────────
            <div class="min-h-0 overflow-hidden bg-black p-sp-2">
                <img class="h-full w-full object-contain" src=preview_url />
            </div>

            // ── Metadata footer ───────────────────────────────────────────
            <div class="max-h-[32vh] overflow-y-auto border-t border-border bg-bg p-sp-4">
                {meta_view}
            </div>

            // ── Mobile-only bottom action bar (<640px) ────────────────────
            <div class="hidden border-t border-border bg-bg-panel-solid px-sp-3 py-sp-2 max-[639px]:grid max-[639px]:grid-cols-4 gap-sp-2">
                <button class="btn btn--sm btn-ghost" on:click=move |_| download_file(&rel_download_mob)>{tr.files_download}</button>
                <button class="btn btn--sm btn-ghost" on:click=move |_| copy_to_clipboard(&rel_copy_mob, flash, tr.files_path_copied)>{tr.files_copy_path}</button>
                <button class="btn btn--sm btn-danger" on:click=move |_| delete_file_action(&rel_delete_mob, refresh_tick, selected, flash, tr)>{tr.files_delete}</button>
                <button class="btn btn--sm btn-ghost text-lg" on:click=move |_| preview_open.set(false)>"×"</button>
            </div>
        </div>
    }.into_any()
}

pub(super) fn render_meta_blocks(m: &FileMeta, tr: &'static Translations) -> impl IntoView + use<> {
    let p = m
        .fits
        .as_ref()
        .map(|f| f.parsed.clone())
        .unwrap_or(Value::Null);
    let header_rows: Vec<FitsRow> = m
        .fits
        .as_ref()
        .map(|f| f.header.clone())
        .unwrap_or_default();
    let has_header = !header_rows.is_empty();

    view! {
        <details class="rounded-md border border-border bg-bg-elev-1" open>
            <summary class="cursor-pointer px-sp-3 py-sp-2 text-sm font-semibold uppercase tracking-[0.06em] text-text-blue">{tr.files_capture_basics}</summary>
            <div class="grid gap-sp-2 p-sp-3">
                {kv(tr.files_filename, m.name.clone())}
                {kv(tr.files_size, format_size(m.size))}
                {kv(tr.files_mtime, format_mtime(m.mtime))}
                {kv(tr.files_exposure, value_or_dash(p.get("exposure")))}
                {kv(tr.files_gain, value_or_dash(p.get("gain")))}
                {kv(tr.files_binning, value_or_dash(p.get("binning")))}
                {kv(tr.files_frame_type, value_or_dash(p.get("frame_type")))}
            </div>
        </details>
        <details class="mt-sp-2 rounded-md border border-border bg-bg-elev-1">
            <summary class="cursor-pointer px-sp-3 py-sp-2 text-sm font-semibold uppercase tracking-[0.06em] text-text-blue">{tr.files_optical}</summary>
            <div class="grid gap-sp-2 p-sp-3">
                {kv(tr.files_filter, value_or_dash(p.get("filter")))}
                {kv(tr.files_target, value_or_dash(p.get("target")))}
                {kv(tr.files_focal, value_or_dash(p.get("focal_length")))}
                {kv(tr.files_pixel_size, value_or_dash(p.get("pixel_size")))}
                {kv(tr.files_temp, value_or_dash(p.get("ccd_temp")))}
            </div>
        </details>
        <details class="mt-sp-2 rounded-md border border-border bg-bg-elev-1">
            <summary class="cursor-pointer px-sp-3 py-sp-2 text-sm font-semibold uppercase tracking-[0.06em] text-text-blue">{tr.files_astrometry}</summary>
            <div class="grid gap-sp-2 p-sp-3">
                {kv(tr.files_ra, value_or_dash(p.get("ra")))}
                {kv(tr.files_dec, value_or_dash(p.get("dec")))}
                {kv(tr.files_fov, fov_str(p.get("fov_arcmin")))}
                {kv(tr.files_rotation, value_or_dash(p.get("rotation")))}
                {kv(tr.files_plate_solved, p.get("plate_solved").and_then(|v| v.as_bool()).map(|b| if b { tr.yes } else { tr.no }.to_string()).unwrap_or_else(|| "—".into()))}
            </div>
        </details>
        <Show when=move || has_header>
            <details class="mt-sp-2 rounded-md border border-border bg-bg-elev-1">
                <summary class="cursor-pointer px-sp-3 py-sp-2 text-sm font-semibold uppercase tracking-[0.06em] text-text-blue">{tr.files_raw_header}</summary>
                <div class="max-h-[280px] overflow-y-auto overflow-x-auto p-sp-3 text-sm font-mono">
                    <div class="min-w-[420px]">
                        {header_rows.iter().map(|r| view! {
                            <div class="grid grid-cols-[80px_1fr_1.2fr] gap-sp-2 border-b border-dotted border-border py-[2px]">
                                <span class="text-text-blue">{r.key.clone()}</span>
                                <span class="break-all text-text-dim">{r.value.clone()}</span>
                                <span class="break-all italic text-text-faint">{r.comment.clone()}</span>
                            </div>
                        }).collect_view()}
                    </div>
                </div>
            </details>
        </Show>
    }
}
