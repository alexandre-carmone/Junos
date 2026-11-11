//! The native Guide tab's Tools sheet: PHD2's secondary tools over
//! `guider_tools`. The Star-Cross Test (`starcross_test.cpp`,
//! `starcross_start {pulse_count?, pulse_size?}` — left empty, the last
//! test's or the guide-speed suggestion), the Adjust Lock Position tool
//! (`nudge_lock.cpp`: `nudge_lock {dx, dy}`, `save_lock`, `restore_lock`
//! on a second tap as PHD2 asks first), the lock-position bookmarks
//! (`bookmark_add {at}`, `bookmark_delete {x, y}`, `bookmarks_clear`) and
//! the Calibrate Meridian Flip tool (`pierflip_tool.cpp`: `pierflip_*`).

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{CARD, CARD_TITLE, LABEL, NUM, ROW};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang};

use super::protocol::{call_error, second_tap, Reply, Rpc};

const GO: &str = "btn btn-primary h-11 px-5 font-semibold";
const STOP: &str = "btn btn-danger h-11 px-5 font-semibold";
const MORE: &str = "btn btn-ghost h-11 px-4";

/// PHD2's position colours (`green`, `yellow`, `red`).
fn colour(c: &Value) -> &'static str {
    match c.as_str() {
        Some("green") => "font-mono text-state-ok",
        Some("yellow") => "font-mono text-state-warn",
        Some("red") => "font-mono text-state-err",
        _ => "font-mono text-text-dim",
    }
}

pub(super) fn tools_sheet(tools: RwSignal<Value>, rpc: Rpc, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    let rpc = StoredValue::new(rpc);
    let last = RwSignal::new(None::<Reply>);
    let call = move |method: &'static str, params: Value| last.set(Some(rpc.with_value(|r| r.call(method, params))));
    let button = move |label: fn(&'static crate::i18n::Translations) -> &'static str, class: &'static str, method: &'static str| view! {
        <button class=class on:click=move |_| call(method, json!({}))>{move || label(tr())}</button>
    };

    // ── Star-cross test ───────────────────────────────────────────────
    let sct = Memo::new(move |_| tools.with(|t| t["starcross"].clone()));
    let sct_running = move || sct.with(|s| s["running"].as_bool().unwrap_or(false));
    let size = RwSignal::new(String::new());
    let count = RwSignal::new(String::new());
    let int = |s: RwSignal<String>| s.with_untracked(|s| s.trim().parse::<i64>().ok());
    let field = move |label: fn(&'static crate::i18n::Translations) -> &'static str, v: RwSignal<String>, key: &'static str| view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label(tr())}</span>
            <input type="number" inputmode="numeric" step="1" class=NUM
                   placeholder=move || sct.with(|s| s[key].as_i64().map(|n| n.to_string()).unwrap_or_default())
                   prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev)) />
        </div>
    };
    let starcross = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{move || tr().guider_sct_title}</span>
            <span class="text-xs text-text-muted">{move || tr().guider_sct_hint}</span>
            {field(|t| t.guider_sct_pulse_size, size, "pulse_size")}
            {field(|t| t.guider_sct_pulse_count, count, "pulse_count")}
            <div class="text-sm text-text-blue break-words">
                {move || sct.with(|s| {
                    let p = &s["progress"];
                    let n = match (p[0].as_i64(), p[1].as_i64()) {
                        (Some(n), Some(total)) if total > 0 => format!("{n}/{total} \u{00b7} "),
                        _ => String::new(),
                    };
                    format!("{n}{}", s["status"].as_str().unwrap_or(""))
                })}
            </div>
            <Show when=sct_running fallback=move || view! {
                <button class=GO on:click=move |_| call("starcross_start", json!({
                    "pulse_count": int(count), "pulse_size": int(size),
                }))>{move || tr().guider_ga_start}</button>
            }>
                {button(|t| t.guider_btn_stop, STOP, "starcross_stop")}
            </Show>
        </div>
    };

    // ── Lock position: nudge, save, restore ───────────────────────────
    let step = RwSignal::new("1".to_string());
    let nudge = move |dx: f64, dy: f64| {
        let s = step.with_untracked(|s| s.trim().parse::<f64>().ok()).unwrap_or(1.0);
        call("nudge_lock", json!({"dx": dx * s, "dy": dy * s}));
    };
    let arrow = move |glyph: &'static str, dx: f64, dy: f64| view! {
        <button class="btn btn-ghost h-11 w-11 text-lg" on:click=move |_| nudge(dx, dy)>{glyph}</button>
    };
    let restore_armed = RwSignal::new(false);
    let lock = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{move || tr().guider_nudge_title}</span>
            <div class="flex items-center gap-4">
                <div class="grid grid-cols-3 gap-1">
                    <span></span>{arrow("\u{2191}", 0.0, -1.0)}<span></span>
                    {arrow("\u{2190}", -1.0, 0.0)}<span></span>{arrow("\u{2192}", 1.0, 0.0)}
                    <span></span>{arrow("\u{2193}", 0.0, 1.0)}<span></span>
                </div>
                <div class="flex flex-col gap-1 min-w-0">
                    <span class=LABEL>{move || tr().guider_nudge_step}</span>
                    <input type="number" inputmode="decimal" step="any" min="0" class=NUM
                           prop:value=move || step.get() on:input=move |ev| step.set(event_target_value(&ev)) />
                </div>
            </div>
            <div class="flex gap-2 flex-wrap">
                {button(|t| t.save_btn, MORE, "save_lock")}
                <button class=move || if restore_armed.get() { "btn btn-danger h-11 px-4" } else { MORE }
                        on:click=move |_| if second_tap(restore_armed) { call("restore_lock", json!({})) }>
                    {move || if restore_armed.get() { tr().guider_tap_to_confirm } else { tr().guider_lock_restore }}
                </button>
            </div>
        </div>
    };

    // ── Bookmarks ─────────────────────────────────────────────────────
    let marks = Memo::new(move |_| {
        tools.with(|t| {
            t["bookmarks"].as_array().into_iter().flatten()
                .filter_map(|b| Some((b[0].as_f64()?, b[1].as_f64()?)))
                .collect::<Vec<_>>()
        })
    });
    let clear_armed = RwSignal::new(false);
    let bookmarks = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{move || tr().guider_bookmarks_title}</span>
            <div class="flex flex-col">
                {move || {
                    let list = marks.get();
                    if list.is_empty() {
                        return view! { <span class="text-sm text-text-faint">{tr().guider_bookmarks_none}</span> }.into_any();
                    }
                    list.into_iter().map(|(x, y)| view! {
                        <div class=ROW>
                            <span class="flex-1 font-mono text-sm text-text-dim">{format!("{x:.1}, {y:.1}")}</span>
                            <button class="btn-icon text-text-muted" title=move || tr().guider_clear
                                    on:click=move |_| call("bookmark_delete", json!({"x": x, "y": y}))>"\u{2716}"</button>
                        </div>
                    }).collect_view().into_any()
                }}
            </div>
            <div class="flex gap-2 flex-wrap">
                <button class=MORE on:click=move |_| call("bookmark_add", json!({"at": "lock"}))>
                    {move || tr().guider_bookmark_lock}
                </button>
                <button class=MORE on:click=move |_| call("bookmark_add", json!({"at": "star"}))>
                    {move || tr().guider_bookmark_star}
                </button>
                <button class=move || if clear_armed.get() { "btn btn-danger h-11 px-4" } else { MORE }
                        disabled=move || marks.with(Vec::is_empty)
                        on:click=move |_| if second_tap(clear_armed) { call("bookmarks_clear", json!({})) }>
                    {move || if clear_armed.get() { tr().guider_tap_to_confirm } else { tr().guider_clear }}
                </button>
            </div>
        </div>
    };

    // ── Meridian flip calibration ─────────────────────────────────────
    let pf = Memo::new(move |_| tools.with(|t| t["pierflip"].clone()));
    let pf_open = move || pf.with(|p| !p.is_null());
    let pierflip = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{move || tr().guider_pierflip_title}</span>
            <Show when=pf_open fallback=move || view! {
                <span class="text-xs text-text-muted">{move || tr().guider_pierflip_hint}</span>
                <div>{button(|t| t.guider_pa_open, GO, "pierflip_start")}</div>
            }>
                <div class="text-sm text-text whitespace-pre-line">
                    {move || pf.with(|p| p["instructions"].as_str().unwrap_or("").to_string())}
                </div>
                <div class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
                    <span class="text-text-muted">{move || tr().dec_label}</span>
                    <span class=move || pf.with(|p| colour(&p["dec_colour"]))>
                        {move || pf.with(|p| p["dec"].as_str().unwrap_or("").to_string())}
                    </span>
                    <span class="text-text-muted">{move || tr().guider_hour_angle}</span>
                    <span class=move || pf.with(|p| colour(&p["ha_colour"]))>
                        {move || pf.with(|p| p["ha"].as_str().unwrap_or("").to_string())}
                    </span>
                    <span class="text-text-muted">{move || tr().guider_pier_side}</span>
                    <span class=move || pf.with(|p| colour(&p["pier_colour"]))>
                        {move || pf.with(|p| p["pier"].as_str().unwrap_or("").to_string())}
                    </span>
                </div>
                <div class="text-sm text-text-blue break-words">
                    {move || pf.with(|p| p["status"].as_str().unwrap_or("").to_string())}
                </div>
                <div class="flex gap-2 flex-wrap">
                    <button class=GO disabled=move || !pf.with(|p| p["next_enabled"].as_bool().unwrap_or(false))
                            on:click=move |_| call("pierflip_next", json!({}))>{move || tr().guider_pierflip_next}</button>
                    {button(|t| t.guider_pierflip_restart, MORE, "pierflip_restart")}
                    {button(|t| t.guider_pa_close, MORE, "pierflip_stop")}
                </div>
            </Show>
        </div>
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 pb-[max(1rem,env(safe-area-inset-bottom))] \
                    grid grid-cols-1 md:grid-cols-2 gap-3 items-start">
            {starcross}
            {lock}
            {bookmarks}
            {pierflip}
            {move || call_error(last).map(|e| view! {
                <div class="md:col-span-2 text-sm text-state-err break-words">{e}</div>
            })}
        </div>
    }
}
