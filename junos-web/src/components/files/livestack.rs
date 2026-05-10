use std::sync::Arc;

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::i18n::{t, Lang, Translations};
use crate::ws::{LiveStackerState, SendCmd};
use crate::ws_helpers::send_cmd;

use super::actions::open_abs_setting_dir;
use super::settings::LiveStackSettings;
use super::types::LiveStackTab;
use super::utils::{fmt_float, kv, url_encode, SMALL_BTN};

#[component]
fn LiveStackPreview(
    state: RwSignal<Option<LiveStackerState>>,
    latest: RwSignal<Option<String>>,
    latest_warning: RwSignal<Option<String>>,
    selected: RwSignal<Option<String>>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    view! {
        <div class="flex flex-col gap-sp-3 p-sp-4">
            <div class="flex items-center justify-between gap-sp-3">
                <span class="text-sm font-semibold uppercase tracking-[0.06em] text-text-blue">{move || tr().livestack_latest_preview}</span>
                {move || render_livestack_badge(state.get())}
            </div>
            {move || render_livestack_status(state.get(), tr())}
            <Show when=move || latest_warning.get().is_some()>
                <div class="text-sm text-state-warn">{move || latest_warning.get().unwrap_or_default()}</div>
            </Show>
            <Show
                when=move || latest.get().is_some()
                fallback=move || view! { <div class="rounded-md border border-border bg-bg-elev-1 p-sp-5 text-center text-sm text-text-faint">{tr().livestack_preview_hint}</div> }
            >
                {move || {
                    let rel = latest.get().unwrap_or_default();
                    let url = format!("/api/files/raw?as=preview&path={}", url_encode(&rel));
                    view! {
                        <button class="block w-full overflow-hidden rounded-md border border-border-strong bg-black p-0" on:click=move |_| selected.set(Some(rel.clone()))>
                            <img class="max-h-[62vh] w-full object-contain" src=url />
                        </button>
                    }
                }}
            </Show>
        </div>
    }
}

#[component]
fn LiveStackControls(
    state: RwSignal<Option<LiveStackerState>>,
    settings: RwSignal<Value>,
    current_path: RwSignal<String>,
    selected: RwSignal<Option<String>>,
    refresh_tick: RwSignal<u32>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let send_init = Arc::clone(&send);
    let send_start = Arc::clone(&send);
    let send_stop = Arc::clone(&send);
    let send_close = Arc::clone(&send);

    view! {
        <div class="flex flex-col gap-sp-3 p-sp-4">
            <div class="flex items-center justify-between gap-sp-3">
                <span class="text-sm font-semibold uppercase tracking-[0.06em] text-text-blue">{move || tr().livestack_title}</span>
                {move || render_livestack_badge(state.get())}
            </div>
            {move || render_livestack_status(state.get(), tr())}
            <div class="flex flex-wrap gap-sp-2">
                <button class=SMALL_BTN on:click=move |_| send_cmd(&send_init, "livestacker_initialize", json!({}))>{move || tr().livestack_init}</button>
                <button class="btn btn--sm btn-primary" on:click=move |_| send_cmd(&send_start, "livestacker_start", json!({}))>{move || tr().livestack_start}</button>
                <button class=SMALL_BTN on:click=move |_| send_cmd(&send_stop, "livestacker_stop", json!({}))>{move || tr().livestack_stop}</button>
                <button class=SMALL_BTN on:click=move |_| send_cmd(&send_close, "livestacker_close", json!({}))>{move || tr().livestack_close}</button>
            </div>
            <div class="grid grid-cols-2 gap-sp-2 max-[700px]:grid-cols-1">
                <button class=SMALL_BTN on:click=move |_| open_abs_setting_dir(settings.get(), "outputDirectory", current_path, selected, refresh_tick)>
                    {move || tr().livestack_open_output}
                </button>
                <button class=SMALL_BTN on:click=move |_| open_abs_setting_dir(settings.get(), "stackingDirectory", current_path, selected, refresh_tick)>
                    {move || tr().livestack_open_input}
                </button>
            </div>
        </div>
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_livestack_workspace(
    selected_folder: Option<String>,
    tab_signal: RwSignal<LiveStackTab>,
    tab: LiveStackTab,
    state: RwSignal<Option<LiveStackerState>>,
    settings: RwSignal<Value>,
    latest: RwSignal<Option<String>>,
    latest_warning: RwSignal<Option<String>>,
    current_path: RwSignal<String>,
    selected: RwSignal<Option<String>>,
    refresh_tick: RwSignal<u32>,
    send_controls: SendCmd,
    send_settings: SendCmd,
    tr: &'static Translations,
) -> impl IntoView {
    let Some(folder) = selected_folder else {
        return view! {
            <div class="panel p-sp-5 text-center text-sm text-text-faint">{tr.files_folder_hint}</div>
        }.into_any();
    };
    view! {
        <div class="panel overflow-hidden">
            <div class="flex items-center justify-between gap-sp-3 border-b border-border px-sp-4 py-sp-3">
                <div class="min-w-0">
                    <div class="text-sm font-semibold uppercase tracking-[0.06em] text-text-blue">{tr.livestack_title}</div>
                    <div class="truncate text-xs text-text-faint">{folder}</div>
                </div>
                {render_livestack_badge(state.get())}
            </div>
            <div class="flex flex-wrap gap-sp-2 border-b border-border bg-bg-elev-1 px-sp-3 py-sp-2">
                {livestack_tab_button(LiveStackTab::Preview, tab_signal, tab, tr.files_subtab_preview)}
                {livestack_tab_button(LiveStackTab::Controls, tab_signal, tab, tr.files_subtab_controls)}
                {livestack_tab_button(LiveStackTab::Settings, tab_signal, tab, tr.files_subtab_settings)}
            </div>
            {match tab {
                LiveStackTab::Preview => view! { <LiveStackPreview state=state latest=latest latest_warning=latest_warning selected=selected /> }.into_any(),
                LiveStackTab::Controls => view! { <LiveStackControls state=state settings=settings current_path=current_path selected=selected refresh_tick=refresh_tick send=send_controls /> }.into_any(),
                LiveStackTab::Settings => view! { <LiveStackSettings settings=settings current_path=current_path send=send_settings /> }.into_any(),
            }}
        </div>
    }.into_any()
}

fn livestack_tab_button(
    kind: LiveStackTab,
    signal: RwSignal<LiveStackTab>,
    active: LiveStackTab,
    label: &'static str,
) -> impl IntoView {
    view! {
        <button
            class=if active == kind { "btn btn--sm btn--active" } else { SMALL_BTN }
            on:click=move |_| signal.set(kind)
        >
            {label}
        </button>
    }
}

pub(super) fn render_livestack_badge(s: Option<LiveStackerState>) -> impl IntoView {
    let (class, label) = match s {
        None => ("badge", "idle".to_string()),
        Some(st) => {
            let lower = st.state.to_ascii_lowercase();
            let class = if lower.contains("error") {
                "badge badge--err"
            } else if lower.contains("run") || lower.contains("loop") {
                "badge badge--ok"
            } else if lower.contains("init") {
                "badge badge--info"
            } else {
                "badge"
            };
            (class, st.state)
        }
    };
    view! { <span class=class>{label}</span> }
}

pub(super) fn render_livestack_status(
    s: Option<LiveStackerState>,
    tr: &'static Translations,
) -> impl IntoView {
    match s {
        None => view! { <span class="text-sm text-text-faint">{tr.livestack_no_state}</span> }
            .into_any(),
        Some(st) => {
            let msg = st.message.clone().unwrap_or_default();
            let msg_visible = msg.clone();
            view! {
            <div class="grid grid-cols-2 gap-sp-2 text-sm max-[700px]:grid-cols-1">
                {kv(tr.livestack_frames, format!("{} / {}", st.frames_stacked, st.total_frames))}
                {kv(tr.livestack_snr, fmt_float(st.mean_snr))}
                {kv(tr.livestack_min_snr, fmt_float(st.min_snr))}
                {kv(tr.livestack_max_snr, fmt_float(st.max_snr))}
                <Show when=move || !msg_visible.is_empty()>
                    <div class="col-span-2 text-sm text-text-muted">{msg.clone()}</div>
                </Show>
            </div>
            }
            .into_any()
        }
    }
}
