//! The shared Filter settings sheet — Ekos' "Filter Settings" dialog for the
//! Capture train's wheel, opened from the Imaging, Focus and Mount headers.
//!
//! One card per slot: name, exposure, focus offset, autofocus on change and
//! the lock filter, edited in a local draft and sent whole with `fm_set_data`
//! (KStars overwrites every column of every row it receives and never
//! answers — the sheet re-asks `fm_get_data` right after). Above the slots:
//! the wheel's position with Move-to chips (INDI `FILTER_SLOT`), the
//! `FlatSyncFocus` option and the door to the Build offsets wizard
//! (`filter_offsets.rs`), which takes over the sheet while it is open.
//!
//! Lock rules mirror `FilterManager` (filtermanager.cpp:170-191): a filter
//! can't lock to itself, a filter that is another's lock can't have a lock,
//! and a lock can only point at a filter without one (`getLockDelegates`).

use gloo_timers::callback::Timeout;
use leptos::prelude::*;
use serde_json::json;

use crate::components::filter_offsets::build_offsets_view;
use crate::components::form::{sheet, CARD, CARD_TITLE, CHECK, CHIP, FOOTER, LABEL, NUM, ROW, SELECT};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};
use crate::ws::{FilterManagerData, FilterRow, SendCmd};
use crate::ws_helpers::{send_cmd, send_device_property_set};
use crate::{FilterCtx, ServiceBusyCtx};

/// KStars' `NULL_FILTER`: no lock.
pub const NO_LOCK: &str = "--";

/// Imaging / Focus / Mount header chip: the filter the wheel sits on (or
/// "Filters" until known), a tap opens the sheet. Nothing without a wheel.
pub fn filter_chip(open: RwSignal<bool>, lang: RwSignal<Lang>) -> impl IntoView {
    let ctx = use_context::<FilterCtx>();
    let name = Memo::new(move |_| ctx.and_then(|c| c.wheel.with(|w| w.current_name().map(str::to_string))));
    let has_wheel = Memo::new(move |_| ctx.is_some_and(|c| c.wheel.with(|w| !w.filter_names.is_empty())));
    view! {
        <Show when=move || has_wheel.get()>
            <button type="button"
                    class="chip h-7 px-2.5 shrink-0 max-w-[110px] cursor-pointer font-mono text-xs"
                    title=move || t(lang.get()).fw_config_title
                    on:click=move |_| open.set(true)>
                <span class="truncate">{move || name.get().unwrap_or_else(|| t(lang.get()).tab_filter_wheel.to_string())}</span>
            </button>
        </Show>
    }
}

/// The sheet itself; each camera tab mounts one inside its pane and drives
/// `open`. Shows the Build offsets wizard instead of the table while
/// `FilterCtx::build_offsets_open` is on.
#[component]
pub fn FilterSettingsSheet(open: RwSignal<bool>, #[prop(into)] send: SendCmd) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let ctx = use_context::<FilterCtx>().expect("FilterCtx not provided");
    let send = StoredValue::new(send);
    // Closing (✖, backdrop or the tab's Escape) leaves the wizard behind,
    // unless a run is on: then reopening from any tab lands on it again.
    let leave_wizard = move || {
        if ctx.offsets_phase.get_untracked() != crate::ws::BfoPhase::Running {
            ctx.build_offsets_open.set(false);
        }
    };
    Effect::new(move |_| {
        if !open.get() {
            leave_wizard();
        }
    });
    let close = move || {
        open.set(false);
        leave_wizard();
    };

    view! {
        <Show when=move || open.get()>
            {move || if ctx.build_offsets_open.get() {
                let back = move || ctx.build_offsets_open.set(false);
                sheet(move || tr().bfo_title, close, build_offsets_view(ctx, back, send.get_value(), lang)).into_any()
            } else {
                sheet(move || tr().fw_config_title, close, table_body(ctx, send.get_value(), lang)).into_any()
            }}
        </Show>
    }
}

// ── Table body ────────────────────────────────────────────────────────────────

fn table_body(ctx: FilterCtx, send: SendCmd, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    // Stored so every handler below captures only Copy values.
    let send = StoredValue::new(send);
    let camera_busy = use_context::<ServiceBusyCtx>().map(|c| c.camera_busy);
    let busy = Memo::new(move |_| camera_busy.is_some_and(|b| b.get().is_some()));

    // The draft follows KStars' table until the first edit; Reset / Save
    // make it the base again, so a 20 s sweep never clobbers a half-edit.
    let draft = RwSignal::new(Vec::<FilterRow>::new());
    let base = RwSignal::new(Vec::<FilterRow>::new());
    let dirty = Memo::new(move |_| draft.with(|d| base.with(|b| d != b)));
    Effect::new(move |_| {
        let rows = ctx.table.with(|t| t.as_ref().map(|t| t.filters.clone()).unwrap_or_default());
        if !dirty.get_untracked() {
            base.set(rows.clone());
            draft.set(rows);
        }
    });
    let n = Memo::new(move |_| draft.with(Vec::len));
    let has_wheel = Memo::new(move |_| ctx.wheel.with(|w| !w.filter_names.is_empty()));
    let error = Memo::new(move |_| draft.with(|d| validate(d)));

    let save = move |_| {
        let Some(device) = ctx.table.with_untracked(|t| t.as_ref().map(|t| t.device.clone())) else { return };
        let rows = draft.get_untracked();
        let s = send.get_value();
        send_cmd(&s, "fm_set_data", FilterManagerData { device, filters: rows.clone() }.to_json());
        base.set(rows);
        // No reply to fm_set_data: ask for the table (and the labels, which
        // KStars rewrites through FILTER_NAME) once it has had time to apply.
        let wheel = ctx.wheel.with_untracked(|w| w.device.clone());
        Timeout::new(500, move || {
            send_cmd(&s, "fm_get_data", json!({}));
            if !wheel.is_empty() {
                send_cmd(&s, "device_property_get", json!({"device": wheel, "property": "FILTER_NAME", "compact": false}));
            }
        })
        .forget();
    };

    let move_to = move |slot: usize| {
        let wheel = ctx.wheel.with_untracked(|w| w.device.clone());
        if !wheel.is_empty() {
            send.with_value(|s| send_device_property_set(s, &wheel, "FILTER_SLOT", json!([{"name": "FILTER_SLOT_VALUE", "value": slot}])));
        }
    };

    let set_flat = move |on: bool| {
        ctx.flat_sync_focus.set(Some(on));
        let s = send.get_value();
        send_cmd(&s, "option_set", json!({"options": [{"name": "FlatSyncFocus", "value": on}]}));
        Timeout::new(500, move || send_cmd(&s, "option_get", json!({"options": [{"name": "FlatSyncFocus"}]}))).forget();
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            // ── Wheel: device, position, Move to ──────────────────────────
            <div class=CARD>
                <div class="flex items-baseline gap-2 min-w-0">
                    <span class=CARD_TITLE>{move || tr().fw_device}</span>
                    <span class="min-w-0 truncate font-mono text-sm text-text-dim">
                        {move || ctx.wheel.with(|w| if w.device.is_empty() { "\u{2014}".to_string() } else { w.device.clone() })}
                    </span>
                </div>
                <Show when=move || has_wheel.get()
                      fallback=move || view! { <span class="text-sm text-text-faint">{move || tr().field_filter_none}</span> }>
                    <div class="flex items-baseline gap-2">
                        <span class=LABEL>{move || tr().fw_current_filter}</span>
                        <span class="font-mono text-base font-semibold text-text-blue-bright">
                            {move || ctx.wheel.with(|w| w.current_name().unwrap_or("\u{2014}").to_string())}
                        </span>
                    </div>
                    <span class=CARD_TITLE>{move || tr().fw_move_to}</span>
                    <div class="flex flex-wrap gap-1.5">
                        {move || {
                            let (names, cur) = ctx.wheel.with(|w| (w.filter_names.clone(), w.current_slot));
                            names.into_iter().enumerate().map(|(i, name)| {
                                let slot = i + 1;
                                let active = cur == Some(slot as i32);
                                view! {
                                    <button type="button"
                                            class=if active { format!("{CHIP} font-mono btn--active") } else { format!("{CHIP} font-mono") }
                                            disabled=move || busy.get()
                                            title=move || if busy.get() { tr().fw_move_busy } else { "" }
                                            on:click=move |_| move_to(slot)>
                                        {name}
                                    </button>
                                }
                            }).collect::<Vec<_>>()
                        }}
                    </div>
                </Show>
            </div>

            // ── Options ───────────────────────────────────────────────────
            <div class=CARD>
                <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                    <input type="checkbox" class=CHECK
                           prop:checked=move || ctx.flat_sync_focus.get().unwrap_or(false)
                           disabled=move || ctx.flat_sync_focus.get().is_none()
                           on:change=move |ev| set_flat(event_target_checked(&ev)) />
                    <span class=LABEL>{move || tr().fw_flat_sync_focus}</span>
                </label>
                <button type="button" class="btn btn-ghost h-10 md:h-9 w-full"
                        disabled=move || dirty.get() || !has_wheel.get()
                        title=move || if dirty.get() { tr().fw_unsaved } else { "" }
                        on:click=move |_| ctx.build_offsets_open.set(true)>
                    {move || tr().fw_build_offsets}
                </button>
            </div>

            // ── One card per slot ─────────────────────────────────────────
            <Show when=move || { n.get() > 0 }
                  fallback=move || view! {
                      <Show when=move || has_wheel.get()>
                          <span class="text-sm text-text-faint px-1">{move || tr().fw_no_table}</span>
                      </Show>
                  }>
                <span class="text-xs text-text-muted px-1">{move || tr().fw_lock_help}</span>
                {move || (0..n.get()).map(|i| slot_card(i, draft, lang)).collect::<Vec<_>>()}
            </Show>
        </div>

        // ── Footer ────────────────────────────────────────────────────────
        <div class=FOOTER>
            <span class="flex-1 min-w-0 truncate text-sm"
                  class:text-state-err=move || error.get().is_some()
                  class:text-text-muted=move || error.get().is_none()>
                {move || match error.get() {
                    Some(label) => label(tr()),
                    None if dirty.get() => tr().fw_unsaved,
                    None => "",
                }}
            </span>
            <button type="button" class="btn btn-ghost h-11 px-4" disabled=move || !dirty.get()
                    on:click=move |_| draft.set(base.get_untracked())>
                {move || tr().reset}
            </button>
            <button type="button" class="btn btn-primary h-11 px-5 font-semibold"
                    disabled=move || !dirty.get() || error.get().is_some()
                    on:click=save>
                {move || tr().fw_save_config}
            </button>
        </div>
    }
}

/// One slot's card. Inputs commit on change (not on input), so KStars' table
/// landing mid-edit can't fight the keyboard.
fn slot_card(i: usize, draft: RwSignal<Vec<FilterRow>>, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    let row = move || draft.with(|d| d.get(i).cloned().unwrap_or_default());
    let edit = move |f: &dyn Fn(&mut FilterRow)| {
        draft.update(|d| {
            if let Some(r) = d.get_mut(i) {
                f(r);
            }
            normalize(d);
        });
    };
    // Lock candidates: the other filters that have no lock of their own.
    let lock_options = move || {
        draft.with(|d| {
            d.iter()
                .enumerate()
                .filter(|(j, r)| *j != i && r.lock == NO_LOCK && !r.label.is_empty())
                .map(|(_, r)| r.label.clone())
                .collect::<Vec<_>>()
        })
    };

    view! {
        <div class=CARD>
            <div class="flex items-center gap-2">
                <span class="shrink-0 font-mono text-xs text-text-faint w-8">{format!("#{}", i + 1)}</span>
                <input type="text" class="input input--sm font-mono flex-1 min-w-0 max-md:h-9"
                       placeholder=move || tr().fw_filter_name
                       prop:value=move || row().label
                       on:change=move |ev| {
                           let new = event_target_value(&ev).trim().to_string();
                           draft.update(|d| {
                               let Some(old) = d.get(i).map(|r| r.label.clone()) else { return };
                               if old == new { return }
                               // Locks follow the renamed filter.
                               for r in d.iter_mut() {
                                   if r.lock == old && !old.is_empty() { r.lock = new.clone(); }
                               }
                               d[i].label = new;
                               normalize(d);
                           });
                       } />
            </div>
            <div class="grid grid-cols-2 gap-2">
                <div class=ROW>
                    <span class=format!("{LABEL} flex-1")>{move || tr().fw_exposure}</span>
                    <input type="number" inputmode="decimal" min="0.001" max="3600" step="any" class=NUM
                           prop:value=move || row().exposure.to_string()
                           on:change=move |ev| {
                               if let Ok(v) = event_target_value(&ev).trim().parse::<f64>() {
                                   if v.is_finite() { edit(&|r| r.exposure = v.clamp(0.001, 3600.0)); }
                               }
                           } />
                </div>
                <div class=ROW>
                    <span class=format!("{LABEL} flex-1")>{move || tr().fw_focus_offset}</span>
                    <input type="number" inputmode="numeric" min="-500000" max="500000" step="1"
                           class="input input--sm font-mono w-[84px] shrink-0 text-right max-md:h-9"
                           prop:value=move || row().offset.to_string()
                           on:change=move |ev| {
                               if let Ok(v) = event_target_value(&ev).trim().parse::<f64>() {
                                   if v.is_finite() { edit(&|r| r.offset = (v.round() as i64).clamp(-500_000, 500_000)); }
                               }
                           } />
                </div>
            </div>
            <div class="flex flex-wrap items-center gap-x-4 gap-y-1">
                <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                    <input type="checkbox" class=CHECK
                           prop:checked=move || row().autofocus
                           on:change=move |ev| { let on = event_target_checked(&ev); edit(&|r| r.autofocus = on); } />
                    <span class=LABEL>{move || tr().fw_autofocus}</span>
                </label>
                <div class=format!("{ROW} flex-1 min-w-[200px]")>
                    <span class=format!("{LABEL} flex-1")>{move || tr().fw_lock}</span>
                    <select class=SELECT
                            on:change=move |ev| { let v = event_target_value(&ev); edit(&|r| r.lock = v.clone()); }>
                        {move || {
                            let cur = row().lock;
                            let mut opts = lock_options();
                            if cur != NO_LOCK && !opts.contains(&cur) { opts.insert(0, cur.clone()); }
                            std::iter::once(view! {
                                <option value=NO_LOCK prop:selected=cur == NO_LOCK>{move || tr().fw_lock_none}</option>
                            }.into_any())
                            .chain(opts.into_iter().map(|o| {
                                let (selected, label) = (o == cur, o.clone());
                                view! { <option value=o prop:selected=selected>{label}</option> }.into_any()
                            }))
                            .collect::<Vec<_>>()
                        }}
                    </select>
                </div>
            </div>
        </div>
    }
}

// ── Rules ─────────────────────────────────────────────────────────────────────

/// Apply KStars' lock rules in row order, the way `FilterManager` would when
/// the rows land: a lock that names a missing filter, the filter itself, a
/// filter that already has a lock, or sits on a filter that is itself a lock
/// for another one, is cleared.
pub fn normalize(rows: &mut [FilterRow]) {
    for i in 0..rows.len() {
        if rows[i].lock.is_empty() {
            rows[i].lock = NO_LOCK.to_string();
        }
        if rows[i].lock == NO_LOCK {
            continue;
        }
        let lock = rows[i].lock.clone();
        let label = rows[i].label.clone();
        let target = rows.iter().position(|r| r.label == lock);
        let is_a_lock = rows.iter().enumerate().any(|(j, r)| j != i && r.lock == label);
        let clear = match target {
            None => true,
            Some(j) if j == i => true,
            Some(j) => rows[j].lock != NO_LOCK,
        } || is_a_lock;
        if clear {
            rows[i].lock = NO_LOCK.to_string();
        }
    }
}

/// What blocks a save: a nameless filter, or two filters with one name
/// (KStars allows it, but locks and every module's picker go by name).
pub fn validate(rows: &[FilterRow]) -> Option<fn(&Translations) -> &'static str> {
    if rows.iter().any(|r| r.label.trim().is_empty()) {
        return Some(|t| t.fw_err_label_empty);
    }
    let mut seen = std::collections::HashSet::new();
    if rows.iter().any(|r| !seen.insert(r.label.as_str())) {
        return Some(|t| t.fw_err_label_dup);
    }
    None
}
