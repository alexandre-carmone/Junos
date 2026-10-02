//! Files tab: the Live Stack sheet, driving KStars' LiveStacker
//! (message.cpp `processLiveStackerCommands`): status and the latest stacked
//! frame, the settings cards (`settings.rs`), and a pinned footer with Init ·
//! Close · Start / Stop.
//!
//! `new_livestacker_state.state` is one of initialized, started, stacking
//! (with frame and SNR stats, `sendLiveStackerProgress`), stopped, closed,
//! error (with a message).

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{CARD, CARD_TITLE, FOOTER};
use crate::i18n::{t, Lang};
use crate::ws::{LiveStackerState, SendCmd};
use crate::ws_helpers::send_cmd;

use super::api::newest_image_in_abs_dir;
use super::utils::{fmt_float, preview_url};
use super::{settings, Shared};

fn running(state: &str) -> bool {
    matches!(state, "started" | "stacking")
}

/// Status dot color, for the header's Live Stack button.
pub(super) fn dot(state: &str) -> &'static str {
    match state {
        "started" | "stacking" => "bg-state-ok",
        "error" => "bg-state-err",
        "initialized" | "stopped" => "bg-state-info",
        _ => "bg-text-faint",
    }
}

fn badge(state: &str) -> &'static str {
    match state {
        "started" | "stacking" => "badge badge--ok",
        "error" => "badge badge--err",
        "initialized" | "stopped" => "badge badge--info",
        _ => "badge",
    }
}

fn stat(label: &'static str, value: String) -> impl IntoView {
    view! {
        <div class="min-w-0 rounded-lg bg-bg-elev-2 border border-border-base px-2.5 py-1.5 flex flex-col">
            <span class="text-xs uppercase tracking-[0.06em] text-text-muted truncate">{label}</span>
            <span class="font-mono tabular-nums text-base leading-tight truncate text-text-blue-bright">{value}</span>
        </div>
    }
}

#[component]
pub(super) fn LiveStack(
    s: Shared,
    /// The sheet; "Open" a folder closes it.
    open: RwSignal<bool>,
    state: RwSignal<Option<LiveStackerState>>,
    settings: RwSignal<Value>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let st = Memo::new(move |_| state.with(|o| o.as_ref().map(|l| l.state.clone()).unwrap_or_default()));

    send_cmd(&send, "livestacker_get_all_settings", json!({}));

    // The newest image in the output folder, re-read on each state push (one
    // per stacked frame). The mtime busts the browser cache.
    let latest = RwSignal::new(None::<String>);
    let warning = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        state.track();
        let out = settings.with(|v| v["outputDirectory"].as_str().unwrap_or_default().to_string());
        if out.is_empty() {
            return;
        }
        let outside = tr().livestack_out_of_sandbox;
        wasm_bindgen_futures::spawn_local(async move {
            match newest_image_in_abs_dir(&out, outside).await {
                Ok(img) => {
                    latest.set(img.map(|(rel, mtime)| format!("{}&v={mtime}", preview_url(&rel))));
                    warning.set(None);
                }
                Err(e) => warning.set(Some(e)),
            }
        });
    });

    let send_cards = send.clone();
    let cmd = move |ty: &'static str| {
        let send = send.clone();
        move |_| send_cmd(&send, ty, json!({}))
    };
    let (on_init, on_close, on_start, on_stop) =
        (cmd("livestacker_initialize"), cmd("livestacker_close"), cmd("livestacker_start"), cmd("livestacker_stop"));

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            <div class=CARD>
                <div class="flex items-center gap-2">
                    <span class=format!("{CARD_TITLE} flex-1")>{move || tr().livestack_state}</span>
                    <span class=move || badge(&st.get())>
                        {move || { let v = st.get(); if v.is_empty() { tr().idle.to_string() } else { v } }}
                    </span>
                </div>
                {move || {
                    let (tr, l) = (tr(), state.get().unwrap_or_default());
                    let message = l.message.clone().filter(|m| !m.is_empty());
                    view! {
                        <div class="grid grid-cols-4 gap-2">
                            {stat(tr.livestack_frames, format!("{} / {}", l.frames_stacked, l.total_frames))}
                            {stat(tr.livestack_snr, fmt_float(l.mean_snr))}
                            {stat(tr.livestack_min_snr, fmt_float(l.min_snr))}
                            {stat(tr.livestack_max_snr, fmt_float(l.max_snr))}
                        </div>
                        {message.map(|m| view! {
                            <div class=if l.state == "error" { "text-sm text-state-err" } else { "text-sm text-text-muted" }>{m}</div>
                        })}
                    }
                }}
            </div>

            // Latest stacked frame; a tap opens it full screen.
            <div class="relative shrink-0 h-[32dvh] min-h-[160px] overflow-hidden rounded-lg border border-border-base \
                        bg-bg-input-deep flex items-center justify-center">
                {move || match latest.get() {
                    Some(url) => view! {
                        <img class="w-full h-full object-contain cursor-zoom-in" src=url.clone() alt=""
                             title=move || tr().livestack_latest_preview
                             on:click=move |_| s.zoom(url.clone()) />
                    }.into_any(),
                    None => view! {
                        <span class="px-6 text-center text-sm text-text-faint">
                            {move || warning.get().unwrap_or_else(|| tr().livestack_preview_hint.to_string())}
                        </span>
                    }.into_any(),
                }}
            </div>

            {settings::cards(s, open, settings, send_cards, lang)}
        </div>

        <div class=FOOTER>
            <button class="btn btn-ghost h-11 px-4 max-md:flex-1" on:click=on_init>{move || tr().livestack_init}</button>
            <button class="btn btn-ghost h-11 px-4 max-md:flex-1" on:click=on_close>{move || tr().livestack_close}</button>
            <Show when=move || running(&st.get())
                  fallback=move || view! {
                      <button class="btn btn-primary h-11 px-5 font-semibold max-md:flex-1 md:ml-auto"
                              on:click=on_start.clone()>
                          {move || format!("\u{25B6}\u{FE0E} {}", tr().livestack_start)}
                      </button>
                  }>
                <button class="btn btn-danger h-11 px-5 font-semibold max-md:flex-1 md:ml-auto" on:click=on_stop.clone()>
                    {move || format!("\u{25A0} {}", tr().livestack_stop)}
                </button>
            </Show>
        </div>
    }
}
