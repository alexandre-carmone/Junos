//! Logs sheet and alerts toast. The sheet has two tabs: the daemon's
//! alerts (`guider_alert`, newest first, at most 50 in the store) and its
//! log files, PHD2's guide and debug logs (`list_logs`), each with a tail
//! view (`read_log {name, tail}`, newest at the bottom) and a download (the
//! whole file through `read_log`, saved from a Blob). The toast shows the
//! newest alert over the bottom of the tab: an info for 8 s, a warning or
//! an error until tapped.

use leptos::prelude::*;
use serde_json::{json, Value};
use wasm_bindgen::JsValue;

use crate::components::form::{CARD, CHIP};
use crate::i18n::{t, Lang};
use crate::ws::GuiderAlert;

use super::protocol::{call_error, Reply, Rpc};
use super::download_text;

/// How long an info alert stays up, ms.
const TOAST_MS: f64 = 8000.0;
/// The tail view's size, bytes.
const TAIL_BYTES: u64 = 16_384;

/// Local `YYYY-MM-DD HH:MM:SS` of `ms` since 1970.
fn local_time(ms: f64) -> String {
    let d = web_sys::js_sys::Date::new(&JsValue::from_f64(ms));
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        d.get_full_year(), d.get_month() + 1, d.get_date(), d.get_hours(), d.get_minutes(), d.get_seconds()
    )
}

/// Text colour and mark of an alert type (`info | question | warning | error`).
fn alert_tone(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "error" => ("text-state-err", "\u{2716}"),
        "warning" => ("text-state-warn", "\u{26a0}"),
        _ => ("text-text", "\u{2139}"),
    }
}

pub(super) fn alert_toast(alerts: RwSignal<Vec<GuiderAlert>>) -> impl IntoView {
    let dismissed = RwSignal::new(0.0_f64);
    // Re-checked once an info alert has had its time.
    let tick = RwSignal::new(0_u32);
    Effect::new(move |_| {
        if alerts.with(|a| !a.is_empty()) {
            gloo_timers::callback::Timeout::new(TOAST_MS as u32 + 50, move || {
                tick.try_update(|n| *n += 1);
            })
            .forget();
        }
    });
    let shown = move || {
        tick.track();
        let now = web_sys::js_sys::Date::now();
        alerts.with(|a| a.first().cloned()).filter(|a| {
            a.t_ms > dismissed.get() && (matches!(a.kind.as_str(), "error" | "warning") || now - a.t_ms < TOAST_MS)
        })
    };
    move || {
        shown().map(|a| {
            let (color, mark) = alert_tone(&a.kind);
            view! {
                <button class=format!("absolute inset-x-4 bottom-[calc(4.5rem_+_env(safe-area-inset-bottom))] z-[60] \
                                       md:left-auto md:right-6 md:w-[420px] panel px-3 py-2 flex items-start gap-2 \
                                       text-left text-sm shadow-lg {color}")
                        on:click=move |_| dismissed.set(a.t_ms)>
                    <span class="shrink-0">{mark}</span>
                    <span class="flex-1 min-w-0 break-words text-text">{a.msg.clone()}</span>
                </button>
            }
        })
    }
}

pub(super) fn logs_sheet(alerts: RwSignal<Vec<GuiderAlert>>, rpc: Rpc, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    let rpc = StoredValue::new(rpc);
    let call = move |method: &'static str, params: Value| rpc.with_value(|r| r.call(method, params));
    let files_tab = RwSignal::new(alerts.with_untracked(Vec::is_empty));

    let list = RwSignal::new(Some(call("list_logs", json!({}))));
    let tail = RwSignal::new(None::<Reply>);
    let tail_name = RwSignal::new(String::new());
    let download = RwSignal::new(None::<(String, Reply)>);
    let last = RwSignal::new(None::<Reply>);
    // A download's whole file, saved once it lands.
    Effect::new(move |_| {
        let Some((name, reply)) = download.get() else { return };
        if let Some(r) = reply.get() {
            if let Ok(v) = &r {
                download_text(&name, v["text"].as_str().unwrap_or(""));
            }
            last.set(Some(reply));
            download.set(None);
        }
    });
    let open_tail = move |name: String| {
        tail.set(Some(call("read_log", json!({"name": name, "tail": TAIL_BYTES}))));
        tail_name.set(name);
    };
    let tail_text = move || {
        let v = tail.get().and_then(|r| r.get()).and_then(Result::ok)?;
        let text = v["text"].as_str().unwrap_or("");
        // A cut tail starts mid-line: drop that part line.
        let cut = v["size"].as_u64().unwrap_or(0) > TAIL_BYTES;
        Some(if cut { text.split_once('\n').map_or(text, |(_, rest)| rest) } else { text }.to_string())
    };
    let rows = move || {
        let files = list.get().and_then(|r| r.get()).and_then(Result::ok).unwrap_or(Value::Null);
        let files = files.as_array().cloned().unwrap_or_default();
        if files.is_empty() {
            return view! { <span class="text-sm text-text-muted">{move || tr().guider_logs_none}</span> }.into_any();
        }
        files.into_iter().map(|f| {
            let name = f["name"].as_str().unwrap_or("").to_string();
            let (n1, n2) = (name.clone(), name.clone());
            let size = f["size"].as_f64().unwrap_or(0.0) / 1024.0;
            let when = local_time(f["modified"].as_f64().unwrap_or(0.0) * 1000.0);
            view! {
                <div class="flex items-center gap-2 min-h-[44px] md:min-h-9 border-b border-border-base last:border-b-0">
                    <div class="flex-1 min-w-0 flex flex-col">
                        <span class="font-mono text-xs text-text-blue truncate" title=name.clone()>{name.clone()}</span>
                        <span class="text-[11px] text-text-muted">{format!("{when} \u{00b7} {size:.0} KB")}</span>
                    </div>
                    <button class="btn btn-ghost h-9 px-3 shrink-0" on:click=move |_| open_tail(n1.clone())>
                        {move || tr().guider_logs_tail}
                    </button>
                    <button class="btn btn-ghost h-9 px-3 shrink-0" title=move || tr().guider_logs_download
                            on:click=move |_| download.set(Some((n2.clone(), call("read_log", json!({"name": n2})))))>
                        "\u{2913}"
                    </button>
                </div>
            }
        }).collect_view().into_any()
    };
    let alert_rows = move || {
        alerts.get().into_iter().map(|a| {
            let (color, mark) = alert_tone(&a.kind);
            view! {
                <div class="flex items-start gap-2 py-1.5 border-b border-border-base last:border-b-0 text-sm">
                    <span class=format!("shrink-0 {color}")>{mark}</span>
                    <span class="flex-1 min-w-0 break-words">{a.msg}</span>
                    <span class="shrink-0 font-mono text-[11px] text-text-muted">{local_time(a.t_ms)[11..].to_string()}</span>
                </div>
            }
        }).collect_view()
    };
    let chip = move |files: bool, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button class=move || if files_tab.get() == files { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                on:click=move |_| files_tab.set(files)>{move || label(tr())}</button>
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 pb-[max(1rem,env(safe-area-inset-bottom))] \
                    flex flex-col gap-3">
            <div class="flex gap-1.5 flex-wrap items-center">
                {chip(false, |t| t.guider_logs_alerts)}
                {chip(true, |t| t.guider_logs_files)}
                <span class="flex-1"></span>
                <Show when=move || files_tab.get()
                      fallback=move || view! {
                          <button class=CHIP disabled=move || alerts.with(Vec::is_empty)
                                  on:click=move |_| alerts.set(Vec::new())>{move || tr().guider_logs_clear}</button>
                      }>
                    <button class=CHIP on:click=move |_| list.set(Some(call("list_logs", json!({}))))>
                        {move || tr().guider_logs_refresh}
                    </button>
                </Show>
            </div>
            <Show when=move || files_tab.get()
                  fallback=move || view! {
                      <div class=CARD>
                          <Show when=move || alerts.with(|a| !a.is_empty())
                                fallback=move || view! { <span class="text-sm text-text-muted">{move || tr().guider_logs_no_alerts}</span> }>
                              {alert_rows}
                          </Show>
                      </div>
                  }>
                <div class=CARD>{rows}</div>
                <Show when=move || tail.with(Option::is_some)>
                    <div class=CARD>
                        <div class="flex items-center gap-2">
                            <span class="flex-1 min-w-0 truncate font-mono text-xs text-text-blue">{move || tail_name.get()}</span>
                            <button class=CHIP on:click=move |_| open_tail(tail_name.get_untracked())>
                                {move || tr().guider_logs_refresh}
                            </button>
                            <button class=CHIP on:click=move |_| tail.set(None)>"\u{2716}"</button>
                        </div>
                        // Reversed column: the scroll starts at the newest line.
                        <div class="max-h-[50dvh] overflow-auto flex flex-col-reverse">
                            <pre class="font-mono text-[11px] leading-snug whitespace-pre text-text">
                                {move || tail_text().unwrap_or_default()}
                            </pre>
                        </div>
                        {move || call_error(tail).map(|e| view! { <div class="text-sm text-state-err break-words">{e}</div> })}
                    </div>
                </Show>
                {move || call_error(list).or_else(|| call_error(last)).map(|e| view! {
                    <div class="text-sm text-state-err break-words">{e}</div>
                })}
            </Show>
        </div>
    }
}
