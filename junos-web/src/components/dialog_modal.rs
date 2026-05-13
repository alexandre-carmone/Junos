//! Modal mirror of KStars' KSMessageBox dialogs.
//!
//! KStars routes its blocking dialogs through Ekos Live as
//! `dialog_get_info` (kstars/auxiliary/ksmessagebox.cpp:275 +
//! kstars/ekos/ekoslive/message.cpp:2472). The web app must surface these
//! to the user and reply with `dialog_get_response {button: "<label>"}` so
//! KStars unblocks. Without this, ops like "park before slewing past
//! meridian" stall waiting for someone at the desktop to click OK.
//!
//! Payload schema: `{title, message, icon, timeout, buttons:[label,..],
//! default}`. KStars sends an empty `{}` when the dialog is dismissed
//! (accepted or rejected) — `apply_ekos_event` clears the signal in that
//! case so the modal disappears in sync.
//!
//! Mnemonic `&` characters are stripped from button labels before display
//! and before sending the response, matching `KSMessageBox::selectResponse`
//! (ksmessagebox.cpp:301).

use leptos::prelude::*;

use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

#[component]
pub fn DialogModal(
    #[prop(into)] dialog: RwSignal<Option<serde_json::Value>>,
    send: SendCmd,
) -> impl IntoView {
    let send = StoredValue::new(send);
    view! {
        <Show when=move || dialog.with(|d| d.is_some())>
            {move || {
                let payload = match dialog.get() { Some(p) => p, None => return ().into_any() };
                let title = payload["title"].as_str().unwrap_or("KStars").to_string();
                let message = payload["message"].as_str().unwrap_or("").to_string();
                let buttons: Vec<String> = payload["buttons"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(strip_mnemonic)).collect())
                    .unwrap_or_default();
                let default_btn = payload["default"].as_str().map(strip_mnemonic);
                let send = send.get_value();

                view! {
                    <div class="fixed inset-0 z-[200] bg-[rgba(2,4,10,0.78)] backdrop-blur-sm flex items-center justify-center p-sp-4">
                        <div class="w-full max-w-[520px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.5)] overflow-hidden flex flex-col">
                            <div class="py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.85)]">
                                <h2 class="text-text-blue text-sm uppercase tracking-[0.08em]">{title}</h2>
                            </div>
                            <div class="p-sp-4 text-sm text-text whitespace-pre-line break-words max-h-[60vh] overflow-y-auto">
                                {message}
                            </div>
                            <div class="flex flex-wrap justify-end gap-sp-2 py-sp-3 px-sp-4 border-t border-border-base bg-[rgba(10,12,20,0.85)]">
                                {buttons.into_iter().map(|label| {
                                    let is_default = default_btn.as_deref() == Some(label.as_str());
                                    let send = send.clone();
                                    let label_for_click = label.clone();
                                    let label_for_text = label.clone();
                                    view! {
                                        <button
                                            class="btn btn--sm"
                                            style=if is_default {
                                                "border-color:var(--state-info);color:var(--state-info);background:rgba(40,80,140,0.18);"
                                            } else {
                                                "border-color:var(--text-blue);color:var(--text-blue);"
                                            }
                                            on:click=move |_| {
                                                send_cmd(
                                                    &send,
                                                    "dialog_get_response",
                                                    serde_json::json!({ "button": label_for_click.clone() }),
                                                );
                                                // KStars will echo an empty
                                                // dialog_get_info on close,
                                                // but clear locally too so
                                                // the modal vanishes immediately
                                                // even if the echo is delayed.
                                                dialog.set(None);
                                            }
                                        >{label_for_text}</button>
                                    }.into_any()
                                }).collect::<Vec<_>>()}
                            </div>
                        </div>
                    </div>
                }.into_any()
            }}
        </Show>
    }
}

/// Qt mnemonic shortcut chars (`&`) are noise on the web. Strip them so
/// labels read naturally and so the response button text matches what
/// `KSMessageBox::selectResponse` compares against (it does the same
/// strip on its side, ksmessagebox.cpp:301).
fn strip_mnemonic(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            // Keep a literal `&&` as a single `&`.
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
            // Otherwise drop the mnemonic marker.
        } else {
            out.push(c);
        }
    }
    out
}
