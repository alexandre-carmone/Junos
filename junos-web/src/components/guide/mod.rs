//! Guiding module UI — full-screen tab.
//!
//! Layout (phone-first, like Focus / Scheduler): a header (state · export
//! log · settings), then cards — RMS, drift plot, target plot, guide frame —
//! in one scrolling column on phones (drift | target side by side from `md`),
//! and a pinned footer with Capture / Loop (internal guider only) and Guide /
//! Stop. Every parameter lives in the settings sheet (`settings.rs`).
//!
//! Wire protocol: see kstars/ekos/ekoslive/message.cpp::processGuideCommands
//! (lines 649-692) for outbound commands, and `new_guide_state` emission at
//! message.cpp:2581-2583.
//!
//! Outbound (browser → KStars):
//!   - guide_start, guide_stop, guide_capture, guide_loop, guide_clear
//!   - guide_get_all_settings (primed + refreshed from ws.rs)
//!   - guide_set_all_settings {<widgetName>: <value>}  (no wrapper — the
//!     map is the payload root, unlike align_set_all_settings which wraps
//!     under {settings:{...}}; see message.cpp:673)
//!   - guide_set_calibration_settings (bulk calibration save, message.cpp:679)
//!   - option_set / option_get for GuiderType and PHD2/LinGuider host+port
//!     (global `Options::` values, not inside guide_get_all_settings).
//!
//! Inbound (KStars → browser):
//!   - new_guide_state {status} — one of the 20 labels in ekos.h:20-40.
//!   - guide_get_all_settings — flat widget map.
//!   - option_get [{name, value}, ...] — reply to our option_get.
//!   - new_preview_image with uuid "+G*" — guide camera frame (Internal
//!     guider only, or PHD2 when its camera matches the Ekos guide camera).
//!
//! `guide_report` is declared in commands.h but has no handler branch in
//! `processGuideCommands` — silently dropped. The log export is therefore
//! client-side: a Blob of `GuideSnapshot.log`.
//!
//! Deliberately NOT wired: dither-now / suspend / resume (not exposed over
//! Ekos Live at all — they are DBUS/Q_SCRIPTABLE only in KStars).

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::compat::GuideSnapshot;
use crate::components::form::{sheet, CARD, CARD_TITLE, FOOTER};
use crate::components::tab_wheel_icons::tab_icon;
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;
use crate::Tab;

mod settings;
mod target;
mod timeline;
use settings::{settings_i64, GuideSettings, GUIDERS};
use target::target_plot;
use timeline::drift_plot;

const EXPORT_ICON: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 4 L12 15 M7.5 10.5 L12 15 L16.5 10.5 M5 19 L19 19"/></svg>"##;

/// Badge class and paint color for a guide state (ekos.h:20-40), shared by
/// the header badge and the drift plot's state ribbon.
fn tone(status: &str) -> (&'static str, &'static str) {
    match status {
        "" | "Idle" | "Aborted" | "Disconnected" => ("badge", "var(--text-faint)"),
        "Calibrating" | "Selecting star" | "Looping" | "Capturing" | "Subtracting"
        | "Subframing" | "Reacquiring" => ("badge badge--warn", "var(--state-warn)"),
        "Calibrated" | "Connected" => ("badge badge--info", "var(--state-info)"),
        "Guiding" => ("badge badge--ok", "var(--state-ok)"),
        "Dithering" | "Dithering successful" | "Manual Dithering" | "Settling" => {
            ("badge badge--info", "var(--accent-cyan)")
        }
        "Calibration error" | "Dithering error" | "Suspended" => ("badge badge--err", "var(--state-err)"),
        _ => ("badge", "var(--text-muted)"),
    }
}

/// Not calibrating, guiding or framing — Start (and Capture / Loop) apply;
/// any other state is stopped with `guide_stop` (guide.cpp::isGuiderActive).
fn is_idle(status: &str) -> bool {
    matches!(
        status,
        "" | "Idle" | "Aborted" | "Connected" | "Calibrated" | "Calibration error" | "Disconnected"
    )
}

/// Green within the accuracy threshold, yellow within 1.5×, red beyond —
/// the target plot's rings, applied to an RMS.
fn rms_cls(v: Option<f64>, accuracy: f64) -> &'static str {
    match v {
        None => "text-text-muted",
        Some(v) if v <= accuracy => "text-state-ok",
        Some(v) if v <= accuracy * 1.5 => "text-state-warn",
        Some(_) => "text-state-err",
    }
}

/// Download the accumulated guide log as a text file.
fn export_guide_log(log: &str) {
    use wasm_bindgen::JsValue;
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
    let parts = web_sys::js_sys::Array::of1(&JsValue::from_str(log));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("text/plain;charset=utf-8");
    let Ok(blob) = web_sys::Blob::new_with_str_sequence_and_options(&parts, &opts) else { return };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else { return };
    // Attached for the click: some browsers ignore it on a detached anchor.
    if let (Ok(a), Some(body)) = (doc.create_element("a"), doc.body()) {
        let a = a.unchecked_into::<web_sys::HtmlAnchorElement>();
        a.set_href(&url);
        a.set_download(&format!("guide-log-{}.txt", web_sys::js_sys::Date::now() as u64 / 1000));
        let _ = body.append_child(&a);
        a.click();
        let _ = body.remove_child(&a);
    }
    let _ = web_sys::Url::revoke_object_url(&url);
}

#[component]
pub fn GuideTab(
    #[prop(into)] guide: Signal<GuideSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // `guide` re-derives the whole snapshot on every read: read it once per
    // change, then split it so a log line or a settings refresh doesn't
    // redraw the plots.
    let snap = Memo::new(move |_| guide.get());
    let status   = Memo::new(move |_| snap.with(|g| g.status.clone()));
    let drift    = Memo::new(move |_| snap.with(|g| g.drift.clone()));
    let history  = Memo::new(move |_| snap.with(|g| g.history.clone()));
    let rms      = Memo::new(move |_| snap.with(|g| (g.ra_rms, g.de_rms)));
    let log      = Memo::new(move |_| snap.with(|g| g.log.clone()));
    let device   = Memo::new(move |_| snap.with(|g| g.device.clone()));
    let preview  = Memo::new(move |_| snap.with(|g| g.preview_url.clone()));
    let settings = Memo::new(move |_| snap.with(|g| g.settings.clone()));
    let options  = Memo::new(move |_| snap.with(|g| g.options.clone()));
    let guider   = Memo::new(move |_| options.with(|o| settings_i64(o, "GuiderType").unwrap_or(0)));
    let accuracy = Memo::new(move |_| {
        settings.with(|s| s.get("guiderAccuracyThreshold").and_then(|v| v.as_f64()).unwrap_or(1.5))
    });
    let idle = move || status.with(|s| is_idle(s));
    // KStars' guide log is newest first (Guide::appendLogText prepends).
    let latest = move || log.with(|l| l.lines().find(|x| !x.trim().is_empty()).unwrap_or("").to_string());

    let settings_open = RwSignal::new(false);
    // Escape closes the settings sheet. forget() the closure (one persistent
    // listener per mount); calls into a disposed RwSignal are a no-op in
    // leptos 0.7, so leftover listeners after a tab switch are harmless.
    {
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if e.key() == "Escape" { settings_open.set(false); }
        });
        if let Some(win) = web_sys::window() {
            let _ = win.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        }
        cb.forget();
    }

    let s_toggle = send.clone();
    let on_toggle = move |_| {
        let ty = if idle() { "guide_start" } else { "guide_stop" };
        send_cmd(&s_toggle, ty, serde_json::json!({}));
    };
    let s_capture = send.clone();
    let on_capture = move |_| send_cmd(&s_capture, "guide_capture", serde_json::json!({}));
    let s_loop = send.clone();
    let on_loop = move |_| send_cmd(&s_loop, "guide_loop", serde_json::json!({}));
    let send_settings = send.clone();

    let stat = move |label: String, value: Option<f64>| view! {
        <div class="min-w-0 rounded-lg bg-bg-elev-2 border border-border-base px-2.5 py-1.5 flex flex-col">
            <span class="text-xs uppercase tracking-[0.06em] text-text-muted truncate">{label}</span>
            <span class=format!("font-mono text-xl leading-tight {}", rms_cls(value, accuracy.get()))>
                {value.map(|v| format!("{v:.2}\u{2033}")).unwrap_or_else(|| "\u{2014}".into())}
            </span>
        </div>
    };

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Guide)></span>
                <span class="min-w-0 truncate font-semibold text-text-blue-bright">{move || tr().tab_guide}</span>
                <span class=move || format!("{} ml-auto min-w-0 truncate", tone(&status.get()).0)>
                    {move || { let s = status.get(); if s.is_empty() { tr().idle.to_string() } else { s } }}
                </span>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().guide_export_log
                        disabled=move || log.with(String::is_empty)
                        on:click=move |_| log.with_untracked(|l| export_guide_log(l))>
                    <span class="inline-block w-5 h-5" inner_html=EXPORT_ICON></span>
                </button>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().guide_settings_title
                        on:click=move |_| settings_open.set(true)>
                    <span class="inline-block w-5 h-5" inner_html=tab_icon(Tab::Profiles)></span>
                </button>
            </div>

            // Cards — one column on phones; RMS and frame full width, drift |
            // target from md.
            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:pl-4 md:pr-6">
                <div class="max-w-[1100px] mx-auto grid gap-3 md:grid-cols-[minmax(0,1fr)_300px]">
                    <div class=format!("{CARD} md:col-span-2")>
                        <div class="grid grid-cols-3 gap-2">
                            {move || {
                                let tr = tr();
                                let (ra, de) = rms.get();
                                let total = ra.zip(de).map(|(a, b)| a.hypot(b));
                                view! {
                                    {stat(format!("{} RMS", tr.ra_label), ra)}
                                    {stat(format!("{} RMS", tr.dec_label), de)}
                                    {stat(tr.guide_total.to_string(), total)}
                                }
                            }}
                        </div>
                        <div class="text-xs text-text-muted truncate">
                            {move || {
                                let kind = GUIDERS.iter().find(|(v, _)| *v == guider.get()).map_or("", |(_, l)| l(tr()));
                                let dev = device.get();
                                if dev.is_empty() { kind.to_string() } else { format!("{kind} \u{00b7} {dev}") }
                            }}
                        </div>
                    </div>

                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().guide_drift_title}</span>
                        {move || drift.with(|d| history.with(|h| drift_plot(d, h, tr())))}
                    </div>

                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().guide_target_title}</span>
                        {move || drift.with(|d| target_plot(d, accuracy.get(), tr().guide_no_drift))}
                    </div>

                    // Guide camera frame (uuid "+G*", kstars media.cpp:753).
                    <Show when=move || preview.with(Option::is_some)>
                        <div class="md:col-span-2 rounded-lg bg-bg-input-deep border border-border-base p-1 flex justify-center">
                            <img src=move || preview.get().unwrap_or_default() alt="guide frame"
                                 class="block max-w-full max-h-[50dvh] object-contain [image-rendering:pixelated]" />
                        </div>
                    </Show>
                </div>
            </div>

            // Footer: latest log line (md+), Capture / Loop (internal guider),
            // Guide / Stop.
            <div class=format!("{FOOTER} md:pl-4 md:pr-6")>
                <span class="max-md:hidden flex-1 min-w-0 truncate font-mono text-xs text-text-muted" title=latest>
                    {latest}
                </span>
                <Show when=move || guider.get() == 0>
                    <button class="btn btn-ghost h-11 px-4 max-md:flex-1" disabled=move || !idle() on:click=on_capture.clone()>
                        {move || tr().guide_capture}
                    </button>
                    <button class="btn btn-ghost h-11 px-4 max-md:flex-1" disabled=move || !idle() on:click=on_loop.clone()>
                        {move || tr().guide_loop}
                    </button>
                </Show>
                <button class=move || if idle() {
                            "btn btn-primary h-11 px-5 font-semibold max-md:flex-1"
                        } else {
                            "btn btn-danger h-11 px-5 font-semibold max-md:flex-1"
                        }
                        on:click=on_toggle>
                    {move || if idle() { tr().guide_btn_start } else { tr().guide_btn_stop }}
                </button>
            </div>

            <Show when=move || settings_open.get()>
                {sheet(move || tr().guide_settings_title, move || settings_open.set(false), view! {
                    <GuideSettings settings=settings options=options status=status
                                   send=send_settings.clone() lang=lang />
                })}
            </Show>
        </div>
    }
}
