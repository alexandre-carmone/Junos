//! Mount tab: position, hold-to-move pad, GoTo / Sync, plate solving.
//!
//! Layout (phone-first, like Guide): a header (device · state · settings),
//! then cards — Position, Motion, GoTo, Plate solve — in one scrolling column
//! on phones (two columns from `md`), and a pinned footer with Park / Unpark,
//! Tracking and Stop, so Stop is always one tap away. Settings (meridian flip,
//! solver) open as a sheet, each change sent at once; the solver timeline and
//! log have their own sheet.
//!
//! Outbound (browser → KStars), message.cpp::processMountCommands and
//! ::processAlignCommands:
//!   - mount_goto_rade / mount_sync_rade {ra, de, isJ2000} — sexagesimal
//!     strings (`dms::fromString`); mount_goto_target {target}
//!   - mount_set_motion {direction: N|S|E|W, action}, mount_set_slew_rate {rate}
//!   - mount_park, mount_unpark, mount_abort, mount_set_tracking {enabled}
//!   - mount_set_all_settings {executeMeridianFlip | meridianFlipOffsetDegrees},
//!     then mount_get_all_settings: KStars doesn't echo mount settings.
//!   - align_solve, align_stop, align_load_and_slew {data: base64, ext}
//!   - align_set_all_settings {<widgetName>: value} — flat; Align echoes its
//!     settings back, which refreshes `align_settings`.
//!   - align_set_astrometry_settings {scale, position, threshold,
//!     rotator_control} — the handler sets all four `Options::` at once and
//!     there is no getter, so they are kept here and always sent together.
//!
//! Inbound: new_mount_state (partial payloads), mount_get_all_settings,
//! new_align_state and align_get_all_settings — all folded by `ws/store.rs`.

use leptos::prelude::*;
use serde_json::{json, Value};
use wasm_bindgen::{closure::Closure, JsCast};

use crate::compat::{FilterWheelSnapshot, MountSnapshot, SolveSnapshot};
use crate::components::coord_input::{CoordInput, CoordMode};
use crate::components::form::{
    check_row, setting_row, sheet, CARD, CARD_TITLE, CHECK, CHIP, FOOTER, LABEL, SELECT,
};
use crate::components::tab_wheel_icons::tab_icon;
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;
use crate::ws_helpers::{dispatch_setting, send_cmd};
use crate::Tab;

type LabelFn = fn(&Translations) -> &'static str;

const DASH: &str = "\u{2014}";
const RATE_LABELS: [&str; 8] = ["G", "1×", "2×", "4×", "8×", "16×", "32×", "MAX"];
const BINNING_OPTIONS: [&str; 4] = ["1x1", "2x2", "3x3", "4x4"];
/// KStars radio groups (align.ui): picking one sends the whole group.
const POST_ACTIONS: [(&str, LabelFn); 3] = [
    ("syncR", |t| t.mount_solve_post_sync),
    ("slewR", |t| t.mount_solve_post_slew),
    ("nothingR", |t| t.mount_solve_post_nothing),
];
const SOLVERS: [(&str, LabelFn); 2] = [
    ("localSolverR", |t| t.mount_solve_solver_local),
    ("remoteSolverR", |t| t.mount_solve_solver_remote),
];

/// Wide enough for the value beside Firefox's spinner (form.rs' NUM isn't).
const NUM: &str = "input input--sm font-mono w-[104px] shrink-0 text-right max-md:h-9";
const PAD: &str = "btn-icon !w-full !h-full text-xl text-text-blue \
                   touch-none select-none [-webkit-touch-callout:none]";
const RATE: &str = "chip h-9 md:h-7 px-0 min-w-0 justify-center font-mono cursor-pointer";
const BIG: &str = "text-xl md:text-2xl text-text-blue-bright";
const SMALL: &str = "text-base text-text-dim";
const LOG_ICON: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M5 7 L19 7 M5 12 L19 12 M5 17 L14 17"/></svg>"##;

// ── Formatters ────────────────────────────────────────────────────────────────

fn fmt_hms(h: f64) -> String {
    let s = (h.rem_euclid(24.0) * 3600.0).round() as u64 % 86_400;
    format!("{:02}h {:02}m {:02}s", s / 3600, s % 3600 / 60, s % 60)
}

fn fmt_dms(deg: f64) -> String {
    let sign = if deg < 0.0 { "-" } else { "+" };
    let s = (deg.abs() * 3600.0).round() as u64;
    format!("{sign}{:02}° {:02}' {:02}\"", s / 3600, s % 3600 / 60, s % 60)
}

fn fmt_az(deg: f64) -> String {
    let m = (deg.rem_euclid(360.0) * 60.0) as u64 % 21_600;
    format!("{:03}° {:02}'", m / 60, m % 60)
}

/// Hour angle, given in degrees, as signed hours and minutes.
fn fmt_ha(deg: f64) -> String {
    let sign = if deg < 0.0 { "-" } else { "+" };
    let m = (deg.abs() / 15.0 * 60.0) as u64;
    format!("{sign}{:02}h {:02}m", m / 60, m % 60)
}

/// HH:MM:SS local time from a JS epoch in ms.
fn fmt_clock(t_ms: f64) -> String {
    let d = web_sys::js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(t_ms));
    format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds())
}

fn or_dash(v: Option<f64>, f: impl Fn(f64) -> String) -> String {
    v.map(f).unwrap_or_else(|| DASH.into())
}

// ── State badges ──────────────────────────────────────────────────────────────

fn mount_badge(m: &MountSnapshot, tr: &Translations) -> (&'static str, &'static str) {
    let s = m.status_str.to_lowercase();
    if !m.connected {
        (tr.disconnected, "badge")
    } else if m.slewing {
        (tr.mount_status_slewing, "badge badge--warn")
    } else if m.tracking {
        (tr.mount_status_tracking, "badge badge--ok")
    } else if m.parked {
        (tr.mount_status_parked, "badge badge--info")
    } else if s.contains("parking") {
        (tr.mount_status_parking, "badge badge--info")
    } else if s.contains("error") {
        (tr.mount_status_error, "badge badge--err")
    } else {
        (tr.mount_status_idle, "badge")
    }
}

/// Badge and text classes for an align state. KStars sends the untranslated
/// `alignStates` label (ekos.h:138, message.cpp::setAlignStatus).
fn align_tone(status: &str) -> (&'static str, &'static str) {
    match status {
        "Complete" | "Successful" => ("badge badge--ok", "text-state-ok"),
        "Failed" | "Aborted" => ("badge badge--err", "text-state-err"),
        "In Progress" | "Syncing" | "Slewing" | "Rotating" => ("badge badge--info", "text-state-info"),
        "Suspended" => ("badge badge--warn", "text-state-warn"),
        _ => ("badge", "text-text-dim"),
    }
}

/// A solve is running: the primary button is Stop.
fn align_busy(status: &str) -> bool {
    matches!(status, "In Progress" | "Syncing" | "Slewing" | "Rotating")
}

// ── Small views ───────────────────────────────────────────────────────────────

/// A small uppercase label over a mono value.
fn readout(label: &'static str, value: String, value_cls: &'static str) -> impl IntoView {
    view! {
        <div class="min-w-0 rounded-lg bg-bg-elev-2 border border-border-base px-2.5 py-1.5 flex flex-col">
            <span class="text-xs uppercase tracking-[0.06em] text-text-muted truncate">{label}</span>
            <span class=format!("font-mono tabular-nums whitespace-nowrap leading-tight {value_cls}")>{value}</span>
        </div>
    }
}

fn banner(text: String) -> impl IntoView {
    view! {
        <div class="md:col-span-2 py-2 px-3 rounded-md border border-state-warn bg-bg-elev-1 \
                    text-state-warn text-sm leading-snug">{text}</div>
    }
}

/// A collapsible settings card (as in the Guide settings sheet).
fn section(title: impl Fn() -> &'static str + Send + 'static, open: bool, body: impl IntoView) -> impl IntoView {
    view! {
        <details class="panel group" open=open>
            <summary class="flex items-center gap-2 px-3 min-h-[44px] md:min-h-10 cursor-pointer select-none \
                            list-none [&::-webkit-details-marker]:hidden">
                <span class=format!("{CARD_TITLE} flex-1")>{move || title()}</span>
                <span class="text-text-faint transition-transform group-open:rotate-90">"\u{203a}"</span>
            </summary>
            <div class="px-3 pb-3 flex flex-col">{body}</div>
        </details>
    }
}

// ── Align settings rows ───────────────────────────────────────────────────────

/// What every align settings row needs.
#[derive(Clone)]
struct Ctx {
    send: SendCmd,
    align: Memo<Value>,
    lang: RwSignal<Lang>,
}

impl Ctx {
    fn label(&self, label: LabelFn) -> impl Fn() -> &'static str + Copy + Send + 'static + use<> {
        let lang = self.lang;
        move || label(t(lang.get()))
    }

    fn set(&self, key: &str, value: Value) {
        dispatch_setting(&self.send, "align_set_all_settings", None, key, value);
    }

    fn str_of(&self, key: &str) -> String {
        self.align.with(|s| match s.get(key) {
            Some(Value::String(v)) => v.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        })
    }
}

/// Number row; a whole `step` sends an integer (QSpinBox), else a float.
fn num_row(c: &Ctx, key: &'static str, label: LabelFn, min: f64, max: f64, step: f64) -> impl IntoView + use<> {
    let (cc, align) = (c.clone(), c.align);
    let int = step.fract() == 0.0;
    setting_row(c.label(label), view! {
        <input type="number" inputmode="decimal" class=NUM
               min=min.to_string() max=max.to_string() step=step.to_string()
               prop:value=move || align.with(|s| s.get(key).and_then(Value::as_f64).map(|v| v.to_string()).unwrap_or_default())
               on:change=move |ev| {
                   let Ok(v) = event_target_value(&ev).parse::<f64>() else { return };
                   let v = v.clamp(min, max);
                   cc.set(key, if int { Value::from(v.round() as i64) } else { Value::from(v) });
               } />
    })
}

fn bool_row(c: &Ctx, key: &'static str, label: LabelFn) -> impl IntoView + use<> {
    let (cc, align) = (c.clone(), c.align);
    view! {
        <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
            <input type="checkbox" class=CHECK
                   prop:checked=move || align.with(|s| s.get(key).and_then(Value::as_bool).unwrap_or(false))
                   on:change=move |ev| cc.set(key, Value::Bool(event_target_checked(&ev))) />
            <span class=LABEL>{c.label(label)}</span>
        </label>
    }
}

/// Combo row; the current value is kept selectable when it isn't offered
/// (blank while unset).
fn select_row<F>(c: &Ctx, key: &'static str, label: LabelFn, options: F) -> impl IntoView + use<F>
where
    F: Fn() -> Vec<String> + Send + 'static,
{
    let (cc, cs) = (c.clone(), c.clone());
    setting_row(c.label(label), view! {
        <select class=SELECT on:change=move |ev| {
            let v = event_target_value(&ev);
            if !v.is_empty() { cc.set(key, Value::String(v)); }
        }>
            {move || {
                let cur = cs.str_of(key);
                let mut opts = options();
                if !opts.contains(&cur) { opts.insert(0, cur.clone()); }
                opts.into_iter().map(|o| {
                    let (selected, text) = (o == cur, o.clone());
                    view! { <option value=o prop:selected=selected>{text}</option> }
                }).collect::<Vec<_>>()
            }}
        </select>
    })
}

fn text_row(c: &Ctx, key: &'static str, label: LabelFn) -> impl IntoView + use<> {
    let (cc, cs) = (c.clone(), c.clone());
    setting_row(c.label(label), view! {
        <input type="text" class="input input--sm font-mono w-[104px] shrink-0 max-md:h-9"
               prop:value=move || cs.str_of(key)
               on:change=move |ev| cc.set(key, Value::String(event_target_value(&ev).trim().to_string())) />
    })
}

/// Pills for a KStars radio group: the picked key goes true, the others false.
fn radio_chips(c: &Ctx, group: &'static [(&'static str, LabelFn)]) -> impl IntoView + use<> {
    view! {
        <div class="flex flex-wrap gap-1.5 py-1">
            {group.iter().map(|&(key, label)| {
                let (send, align) = (c.send.clone(), c.align);
                view! {
                    <button type="button"
                            class=move || if align.with(|s| s.get(key).and_then(Value::as_bool).unwrap_or(false)) {
                                format!("{CHIP} btn--active")
                            } else {
                                CHIP.to_string()
                            }
                            on:click=move |_| {
                                let map = group.iter().map(|&(k, _)| (k.to_string(), Value::Bool(k == key))).collect();
                                send_cmd(&send, "align_set_all_settings", Value::Object(map));
                            }>
                        {c.label(label)}
                    </button>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}

// ── MountTab ──────────────────────────────────────────────────────────────────

#[component]
pub fn MountTab(
    mount: Signal<MountSnapshot>,
    solve: Signal<SolveSnapshot>,
    align_settings: RwSignal<Value>,
    filter_wheel: Signal<FilterWheelSnapshot>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // `mount` re-derives on every coordinate tick (~1 s): read it once per
    // change, and give each control its own memo so a tick doesn't redraw the
    // header or rewrite a settings field being typed in.
    let snap = Memo::new(move |_| mount.get());
    let device = Memo::new(move |_| snap.with(|m| m.device_name.clone().unwrap_or_default()));
    let badge = Memo::new(move |_| snap.with(|m| mount_badge(m, tr())));
    let connected = Memo::new(move |_| snap.with(|m| m.connected));
    let parked = Memo::new(move |_| snap.with(|m| m.parked));
    let tracking = Memo::new(move |_| snap.with(|m| m.tracking));
    let rate = Memo::new(move |_| snap.with(|m| m.slew_rate));
    let flip_on = Memo::new(move |_| snap.with(|m| m.meridian_flip_enabled.unwrap_or(false)));
    let flip_offset = Memo::new(move |_| snap.with(|m| m.meridian_flip_offset_deg));
    let align = Memo::new(move |_| align_settings.get());
    let solve_status = Memo::new(move |_| solve.with(|s| s.status.clone().unwrap_or_default()));
    let solving = move || solve_status.with(|s| align_busy(s));

    let settings_open = RwSignal::new(false);
    let log_open = RwSignal::new(false);
    // Escape closes the sheets. forget() the closure (one persistent listener
    // per mount); calls into a disposed RwSignal are a no-op in leptos 0.7, so
    // leftover listeners after a tab switch are harmless.
    {
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if e.key() == "Escape" {
                settings_open.set(false);
                log_open.set(false);
            }
        });
        if let Some(win) = web_sys::window() {
            let _ = win.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        }
        cb.forget();
    }

    // ── Actions ──────────────────────────────────────────────────────────
    let s_plain = send.clone();
    let plain = move |ty: &'static str| {
        let s = s_plain.clone();
        move |_| send_cmd(&s, ty, json!({}))
    };
    let s_park = send.clone();
    let on_park = move |_| {
        let ty = if parked.get_untracked() { "mount_unpark" } else { "mount_park" };
        send_cmd(&s_park, ty, json!({}));
    };
    let s_track = send.clone();
    let on_track = move |_| {
        send_cmd(&s_track, "mount_set_tracking", json!({ "enabled": !tracking.get_untracked() }));
    };
    let s_rate = send.clone();
    let set_rate = move |rate: i32| send_cmd(&s_rate, "mount_set_slew_rate", json!({ "rate": rate }));

    // Hold-to-move. Only the held direction is ever stopped, so a mouse
    // passing over the pad sends nothing; release, leave and `pointercancel`
    // (the browser taking the touch) all stop it, and so does leaving the tab.
    let held = RwSignal::new(None::<&'static str>);
    let s_motion = send.clone();
    let motion = move |dir: &'static str, on: bool| {
        send_cmd(&s_motion, "mount_set_motion", json!({ "direction": dir, "action": on }));
    };
    let release = {
        let motion = motion.clone();
        move |dir: &'static str| {
            if held.get_untracked() == Some(dir) {
                held.set(None);
                motion(dir, false);
            }
        }
    };
    let press = {
        let motion = motion.clone();
        move |dir: &'static str| {
            if let Some(old) = held.get_untracked().filter(|d| *d != dir) {
                motion(old, false);
            }
            held.set(Some(dir));
            motion(dir, true);
        }
    };
    {
        let motion = motion.clone();
        on_cleanup(move || {
            if let Some(dir) = held.try_get_untracked().flatten() {
                motion(dir, false);
            }
        });
    }
    let pad = move |dir: &'static str, glyph: &'static str| {
        let (p, up, cancel, leave) = (press.clone(), release.clone(), release.clone(), release.clone());
        view! {
            <button type="button" aria-label=dir
                    class=move || if held.get() == Some(dir) { format!("{PAD} btn--active") } else { PAD.to_string() }
                    on:pointerdown=move |ev: web_sys::PointerEvent| {
                        if ev.button() != 0 { return; }
                        ev.prevent_default();
                        p(dir);
                    }
                    on:pointerup=move |_| up(dir)
                    on:pointercancel=move |_| cancel(dir)
                    on:pointerleave=move |_| leave(dir)
                    on:contextmenu=|ev: web_sys::MouseEvent| ev.prevent_default()>
                {glyph}
            </button>
        }
    };

    // GoTo / Sync. CoordInput keeps the canonical "HH MM SS" / "+DD MM SS".
    let ra = RwSignal::new(String::new());
    let dec = RwSignal::new(String::new());
    let j2000 = RwSignal::new(false);
    let target = RwSignal::new(String::new());
    let s_rade = send.clone();
    let rade = move |ty: &'static str| {
        let s = s_rade.clone();
        move |_| send_cmd(&s, ty, json!({
            "ra": ra.get_untracked(), "de": dec.get_untracked(), "isJ2000": j2000.get_untracked(),
        }))
    };
    let s_target = send.clone();
    let on_goto_target = move |ev: web_sys::SubmitEvent| {
        ev.prevent_default();
        let name = target.get_untracked().trim().to_string();
        if !name.is_empty() {
            send_cmd(&s_target, "mount_goto_target", json!({ "target": name }));
        }
    };

    // Plate solve.
    let s_solve = send.clone();
    let on_solve = move |_| {
        let ty = if solve_status.with_untracked(|s| align_busy(s)) { "align_stop" } else { "align_solve" };
        send_cmd(&s_solve, ty, json!({}));
    };
    let file_input: NodeRef<leptos::html::Input> = NodeRef::new();
    let s_fits = send.clone();
    let on_file = move |ev: web_sys::Event| {
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        if let Some(file) = input.files().and_then(|f| f.get(0)) {
            load_and_slew(&s_fits, file);
        }
        // Reset so picking the same file again fires `change`.
        input.set_value("");
    };

    // ── Settings ─────────────────────────────────────────────────────────
    let s_mount = send.clone();
    let set_mount = move |key: &str, value: Value| {
        dispatch_setting(&s_mount, "mount_set_all_settings", None, key, value);
        send_cmd(&s_mount, "mount_get_all_settings", json!({}));
    };
    let set_flip = set_mount.clone();
    let set_offset = set_mount.clone();
    // Astrometry hints, seeded with the kstars.kcfg defaults.
    let hint_scale = RwSignal::new(true);
    let hint_position = RwSignal::new(true);
    let hint_threshold = RwSignal::new(30_i64);
    let hint_rotator = RwSignal::new(true);
    let s_hints = send.clone();
    let send_hints = move || send_cmd(&s_hints, "align_set_astrometry_settings", json!({
        "scale":           hint_scale.get_untracked(),
        "position":        hint_position.get_untracked(),
        "threshold":       hint_threshold.get_untracked(),
        "rotator_control": hint_rotator.get_untracked(),
    }));
    let c = Ctx { send: send.clone(), align, lang };

    let settings_body = move || {
        let (h1, h2, h3, h4) = (send_hints.clone(), send_hints.clone(), send_hints.clone(), send_hints.clone());
        let (set_flip, set_offset) = (set_flip.clone(), set_offset.clone());
        view! {
            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 \
                        pb-[max(0.75rem,env(safe-area-inset-bottom))] flex flex-col gap-3">
                {section(move || tr().mount_meridian_flip, true, view! {
                    <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                        <input type="checkbox" class=CHECK
                               prop:checked=move || flip_on.get()
                               on:change=move |ev| set_flip("executeMeridianFlip", Value::Bool(event_target_checked(&ev))) />
                        <span class=LABEL>{move || tr().mount_auto_flip}</span>
                    </label>
                    {setting_row(move || tr().mount_past_meridian, view! {
                        <input type="number" inputmode="decimal" min="0" max="120" step="0.1" class=NUM
                               prop:value=move || flip_offset.get().map(|v| format!("{v:.1}")).unwrap_or_default()
                               on:change=move |ev| {
                                   let Ok(v) = event_target_value(&ev).parse::<f64>() else { return };
                                   set_offset("meridianFlipOffsetDegrees", Value::from(v.clamp(0.0, 120.0)));
                               } />
                        <span class="w-3 text-sm text-text-muted">"\u{00b0}"</span>
                    })}
                })}
                {section(move || tr().mount_solve_params_capture, true, view! {
                    {num_row(&c, "alignExposure", |t| t.align_exposure, 0.1, 300.0, 0.1)}
                    {select_row(&c, "alignBinning", |t| t.mount_solve_binning,
                                || BINNING_OPTIONS.map(String::from).to_vec())}
                    {select_row(&c, "alignFilter", |t| t.mount_solve_filter,
                                move || filter_wheel.with(|f| f.filter_names.clone()))}
                    {num_row(&c, "alignGain", |t| t.mount_solve_gain, 0.0, 10000.0, 0.1)}
                    {text_row(&c, "alignISO", |t| t.mount_solve_iso)}
                    {bool_row(&c, "alignDarkFrame", |t| t.mount_solve_dark_frame)}
                })}
                {section(move || tr().mount_solve_params_solver, false, view! {
                    {num_row(&c, "alignAccuracyThreshold", |t| t.align_accuracy, 1.0, 1200.0, 1.0)}
                    {num_row(&c, "alignSettlingTime", |t| t.mount_solve_settling_ms, 0.0, 15000.0, 100.0)}
                    <span class=format!("{LABEL} pt-2")>{move || tr().mount_solve_post_action}</span>
                    {radio_chips(&c, &POST_ACTIONS)}
                    <span class=format!("{LABEL} pt-2")>{move || tr().mount_solve_solver_source}</span>
                    {radio_chips(&c, &SOLVERS)}
                })}
                {section(move || tr().mount_solve_params_astrometry, false, view! {
                    {check_row(hint_scale, move || tr().mount_solve_use_scale, move |_| h1())}
                    {check_row(hint_position, move || tr().mount_solve_use_position, move |_| h2())}
                    {setting_row(move || tr().mount_solve_rotator_threshold, view! {
                        <input type="number" inputmode="numeric" min="0" step="1" class=NUM
                               prop:value=move || hint_threshold.get().to_string()
                               on:change=move |ev| {
                                   let Ok(v) = event_target_value(&ev).parse::<i64>() else { return };
                                   hint_threshold.set(v.max(0));
                                   h3();
                               } />
                    })}
                    {check_row(hint_rotator, move || tr().mount_solve_rotator_control, move |_| h4())}
                })}
            </div>
        }
    };

    let log_body = move || view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 \
                    pb-[max(0.75rem,env(safe-area-inset-bottom))] flex flex-col gap-3">
            {move || solve.with(|s| s.download_progress.clone()).map(|dp| view! {
                <div class="py-2 px-3 rounded-md border border-state-info text-state-info text-sm">
                    {format!("{}: {dp}", tr().mount_solve_download)}
                </div>
            })}
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().mount_solve_timeline}</span>
                <div class="flex flex-col gap-0.5 max-h-[30dvh] overflow-y-auto">
                    {move || {
                        let history = solve.with(|s| s.history.clone());
                        if history.is_empty() {
                            return view! { <span class="text-sm text-text-faint">{DASH}</span> }.into_any();
                        }
                        history.into_iter().rev().map(|e| {
                            let cls = align_tone(&e.status).1;
                            view! {
                                <div class="flex gap-3 font-mono text-xs">
                                    <span class="text-text-faint tabular-nums">{fmt_clock(e.t_ms)}</span>
                                    <span class=cls>{e.status}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>().into_any()
                    }}
                </div>
            </div>
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().mount_solve_log}</span>
                <pre class="m-0 max-h-[45dvh] overflow-auto font-mono text-xs leading-[1.5] text-text-dim \
                            whitespace-pre-wrap break-words">
                    {move || {
                        let log = solve.with(|s| s.log.clone());
                        if log.trim().is_empty() { tr().mount_solve_no_log.to_string() } else { log }
                    }}
                </pre>
            </div>
        </div>
    };

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Mount)></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{move || tr().tab_mount}</span>
                <span class="min-w-0 truncate text-sm text-text-muted">{move || device.get()}</span>
                <span class=move || format!("{} ml-auto shrink-0", badge.get().1)>{move || badge.get().0}</span>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().mount_settings
                        on:click=move |_| settings_open.set(true)>
                    <span class="inline-block w-5 h-5" inner_html=tab_icon(Tab::Profiles)></span>
                </button>
            </div>

            // Cards — one column on phones, two from md.
            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:pl-4 md:pr-6">
                <div class="max-w-[1100px] mx-auto grid gap-3 md:grid-cols-2">
                    // KStars sends "00:00:00" once the countdown stops.
                    {move || {
                        let park = snap.with(|m| m.auto_park_countdown.clone());
                        (!park.is_empty() && park != "00:00:00")
                            .then(|| banner(format!("{}: {park}", tr().mount_autopark)))
                    }}

                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().mount_coords_section}</span>
                        {move || {
                            let tr = tr();
                            snap.with(|m| {
                                if !m.connected {
                                    return view! {
                                        <div class="py-6 text-center text-sm text-text-faint">{tr.mount_no_device}</div>
                                    }.into_any();
                                }
                                let pier = match m.pier_side {
                                    Some(0) => tr.mount_pier_west,
                                    Some(1) => tr.mount_pier_east,
                                    _ => tr.mount_pier_unknown,
                                };
                                view! {
                                    <div class="grid grid-cols-2 gap-2">
                                        {readout(tr.mount_ra_jnow, or_dash(m.ra_h, fmt_hms), BIG)}
                                        {readout(tr.mount_dec_jnow, or_dash(m.dec_deg, fmt_dms), BIG)}
                                        {readout(tr.mount_alt, or_dash(m.alt_deg, fmt_dms), SMALL)}
                                        {readout(tr.mount_az, or_dash(m.az_deg, fmt_az), SMALL)}
                                        {readout(tr.mount_ha, or_dash(m.ha_deg, fmt_ha), SMALL)}
                                        {readout(tr.mount_pier_side, pier.to_string(), SMALL)}
                                    </div>
                                    <div class="font-mono text-xs text-text-muted truncate">
                                        {format!("{}  {} \u{00b7} {}", tr.mount_j2000_label,
                                                 or_dash(m.ra0_h, fmt_hms), or_dash(m.dec0_deg, fmt_dms))}
                                    </div>
                                    // KStars' own meridian-flip line ("Meridian flip in …").
                                    {(!m.meridian_flip_status.is_empty()).then(|| view! {
                                        <div class="text-xs text-text-muted truncate">{m.meridian_flip_status.clone()}</div>
                                    })}
                                }.into_any()
                            })
                        }}
                    </div>

                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().mount_motion_hold}</span>
                        <div class="grid grid-cols-[repeat(3,4rem)] grid-rows-[repeat(3,4rem)] \
                                    md:grid-cols-[repeat(3,3.5rem)] md:grid-rows-[repeat(3,3.5rem)] gap-2 justify-center py-1">
                            <span></span>
                            {pad("N", "\u{2191}")}
                            <span></span>
                            {pad("W", "\u{2190}")}
                            <button type="button" class="btn-icon btn-danger !w-full !h-full !rounded-pill text-lg"
                                    title=move || tr().mount_abort_btn on:click=plain("mount_abort")>
                                "\u{25A0}"
                            </button>
                            {pad("E", "\u{2192}")}
                            <span></span>
                            {pad("S", "\u{2193}")}
                            <span></span>
                        </div>
                        <span class="text-xs text-text-muted">{move || tr().mount_slew_rate}</span>
                        <div class="grid grid-cols-8 gap-1">
                            {(0..8i32).map(|i| {
                                let set = set_rate.clone();
                                view! {
                                    <button type="button"
                                            class=move || if rate.get() == Some(i) { format!("{RATE} btn--active") } else { RATE.to_string() }
                                            on:click=move |_| set(i)>
                                        {RATE_LABELS[i as usize]}
                                    </button>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </div>

                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().mount_goto_section}</span>
                        <div class="flex flex-wrap items-center gap-x-4 gap-y-2">
                            <div class="flex items-center gap-2">
                                <span class="w-7 text-sm text-text-blue">{move || tr().ra_label}</span>
                                <CoordInput mode=CoordMode::Hms value=ra aria_label="RA" />
                            </div>
                            <div class="flex items-center gap-2">
                                <span class="w-7 text-sm text-text-blue">{move || tr().dec_label}</span>
                                <CoordInput mode=CoordMode::DmsSigned value=dec aria_label="Dec" />
                            </div>
                        </div>
                        <div class="flex items-center gap-2">
                            <button type="button"
                                    class=move || if j2000.get() { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                                    aria-pressed=move || j2000.get().to_string()
                                    on:click=move |_| j2000.update(|v| *v = !*v)>
                                {move || tr().mount_j2000_label}
                            </button>
                            <button class="btn btn-ghost h-11 px-4 ml-auto" on:click=rade("mount_sync_rade")>
                                {move || tr().mount_sync_btn}
                            </button>
                            <button class="btn btn-primary h-11 px-5" on:click=rade("mount_goto_rade")>
                                {move || tr().mount_goto_btn}
                            </button>
                        </div>
                        <form class="flex items-center gap-2 pt-1 border-t border-border-base" on:submit=on_goto_target>
                            <input type="text" class="input flex-1 min-w-0 h-11 md:h-9 mt-2"
                                   placeholder=move || format!("{} \u{2014} M42, NGC 7000\u{2026}", tr().mount_target_input)
                                   prop:value=move || target.get()
                                   on:input=move |ev| target.set(event_target_value(&ev)) />
                            <button type="submit" class="btn btn-primary h-11 md:h-9 px-5 mt-2"
                                    disabled=move || target.with(|s| s.trim().is_empty())>
                                {move || tr().mount_goto_btn}
                            </button>
                        </form>
                    </div>

                    <div class=CARD>
                        <div class="flex items-center gap-2">
                            <span class=format!("{CARD_TITLE} flex-1")>{move || tr().mount_solve_section}</span>
                            <span class=move || align_tone(&solve_status.get()).0>
                                {move || { let s = solve_status.get(); if s.is_empty() { tr().idle.to_string() } else { s } }}
                            </span>
                            <button class="btn-icon shrink-0 text-text-muted" title=move || tr().mount_solve_log
                                    on:click=move |_| log_open.set(true)>
                                <span class="inline-block w-5 h-5" inner_html=LOG_ICON></span>
                            </button>
                        </div>
                        {move || {
                            let tr = tr();
                            solve.with(|s| {
                                if s.ra_jnow_deg.is_none() && s.dec_jnow_deg.is_none() {
                                    return view! {
                                        <div class="py-3 text-center text-sm text-text-faint">{tr.mount_solve_no_solution}</div>
                                    }.into_any();
                                }
                                view! {
                                    <div class="grid grid-cols-2 gap-2">
                                        {readout(tr.mount_ra_jnow, or_dash(s.ra_jnow_deg, |d| fmt_hms(d / 15.0)), SMALL)}
                                        {readout(tr.mount_dec_jnow, or_dash(s.dec_jnow_deg, fmt_dms), SMALL)}
                                        {readout(tr.mount_solve_pa, or_dash(s.rotation_deg, |v| format!("{v:.2}\u{00b0}")), SMALL)}
                                        {readout(tr.mount_solve_pixscale, or_dash(s.pixscale_arcsec, |v| format!("{v:.2}\u{2033}/px")), SMALL)}
                                    </div>
                                    {s.solved_at_ms.map(|t| view! {
                                        <div class="text-xs text-text-muted">
                                            {format!("{} {}", tr.mount_solve_solved_at, fmt_clock(t))}
                                        </div>
                                    })}
                                }.into_any()
                            })
                        }}
                        <div class="flex gap-2">
                            <button class="btn btn-ghost h-11 flex-1"
                                    on:click=move |_| if let Some(el) = file_input.get() { el.click() }>
                                {move || tr().mount_solve_load_fits}
                            </button>
                            <button class=move || if solving() { "btn btn-danger h-11 flex-1" } else { "btn btn-primary h-11 flex-1" }
                                    on:click=on_solve>
                                {move || if solving() { tr().mount_solve_stop } else { tr().mount_solve_capture }}
                            </button>
                        </div>
                        <input node_ref=file_input type="file" accept=".fits,.fit,.fts" class="hidden" on:change=on_file />
                    </div>
                </div>
            </div>

            // Footer: Park / Unpark, Tracking, Stop.
            <div class=format!("{FOOTER} md:pl-4 md:pr-6")>
                <button class="btn btn-ghost h-11 px-4 max-md:flex-1" disabled=move || !connected.get() on:click=on_park>
                    {move || if parked.get() { tr().mount_unpark_btn } else { tr().mount_park_btn }}
                </button>
                <button class=move || if tracking.get() {
                            "btn btn-ghost btn--active h-11 px-4 max-md:flex-1"
                        } else {
                            "btn btn-ghost h-11 px-4 max-md:flex-1"
                        }
                        aria-pressed=move || tracking.get().to_string()
                        disabled=move || !connected.get() on:click=on_track>
                    <span class=move || if tracking.get() {
                        "w-2 h-2 rounded-full bg-state-ok"
                    } else {
                        "w-2 h-2 rounded-full bg-text-faint"
                    }></span>
                    {move || tr().mount_tracking_on}
                </button>
                <button class="btn btn-danger h-11 px-5 font-semibold md:ml-auto max-md:flex-1" on:click=plain("mount_abort")>
                    {move || format!("\u{25A0} {}", tr().mount_abort_btn)}
                </button>
            </div>

            <Show when=move || settings_open.get()>
                {sheet(move || tr().mount_settings, move || settings_open.set(false), settings_body())}
            </Show>
            <Show when=move || log_open.get()>
                {sheet(move || tr().mount_solve_log, move || log_open.set(false), log_body())}
            </Show>
        </div>
    }
}

/// Read a FITS file and send it base64-encoded as `align_load_and_slew`.
fn load_and_slew(send: &SendCmd, file: web_sys::File) {
    let ext = file.name().rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_else(|| "fits".into());
    let Ok(reader) = web_sys::FileReader::new() else { return };
    let (r, send) = (reader.clone(), send.clone());
    let onload = Closure::<dyn FnMut()>::new(move || {
        let Ok(buf) = r.result() else { return };
        // btoa wants a binary string: one char per byte.
        let bin: String = web_sys::js_sys::Uint8Array::new(&buf).to_vec().into_iter().map(char::from).collect();
        let Some(b64) = web_sys::window().and_then(|w| w.btoa(&bin).ok()) else { return };
        send_cmd(&send, "align_load_and_slew", json!({ "data": b64, "ext": ext }));
    });
    reader.set_onload(Some(onload.as_ref().unchecked_ref()));
    onload.forget();
    let _ = reader.read_as_array_buffer(&file);
}
