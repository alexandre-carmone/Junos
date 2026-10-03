//! Scheduler: the startup/shutdown queue editor — the detail pane of the
//! Startup & shutdown sub-tab (`view_procedures.rs`).
//!
//! Opens for one [`QueueSlot`]. It loads the managed queue the slot already
//! points at — or starts from the slot's stock preset — and lets the user add,
//! reorder and remove steps: built-in steps, custom INDI steps
//! (`view_indi_step.rs`) and shell scripts edited inline, with snippets.
//! Saving writes the scripts and the queue through junos-server
//! (`/api/taskqueue/*`), then points the slot at the queue with
//! `scheduler_set_all_settings`.

use std::collections::HashMap;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::labels::{param_label, snippet_label, step_label};
use super::queue_api::{self, QueueList, SaveErr};
use super::queue_model::{
    fmt_num, from_document, is_safe_name, managed_script_path, template, to_document, validate,
    ParamSpec, Preset, QueueError, QueueSlot, QueueStep, ScriptRef, FAIL_ABORT, FAIL_CONTINUE,
    FAIL_SKIP, NEW_SCRIPT_BODY, SCRIPT_TIMEOUT,
};
use super::queue_native::{IndiOp, IndiStep};
use super::queue_snippets::SNIPPETS;
use super::view_indi_step::{indi_body, IndiRow, IndiSource};
use crate::components::form::{CARD_TITLE, CHIP, FOOTER};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};
use crate::ws::{DeviceInfo, IndiProperty, SendCmd};
use crate::ws_helpers::send_cmd;

pub(super) const FIELDS: &str = "flex flex-wrap items-center gap-x-3 gap-y-2";
pub(super) const FIELD_LABEL: &str = "text-sm text-text-blue";
const UNIT: &str = "text-sm text-text-faint";
const INPUT: &str = "input input--sm font-mono max-md:h-9";
pub(super) const SELECT: &str = "input input--sm max-md:h-9";

/// The Add step picker: groups of step choices — template ids, plus
/// `indi_set` / `indi_wait` / `script_new` / `script_ext`.
const ADD_GROUPS: &[(&str, &[&str])] = &[
    ("mount",  &["mount_unpark", "mount_park"]),
    ("dome",   &["dome_unpark", "dome_park"]),
    ("cap",    &["dustcap_unpark", "dustcap_park"]),
    ("camera", &["camera_cool", "camera_warm", "camera_warm_passive"]),
    ("indi",   &["indi_set", "indi_wait"]),
    ("other",  &["delay", "script_new", "script_ext"]),
];

fn group_label(tr: &'static Translations, group: &str) -> &'static str {
    match group {
        "mount"  => tr.sched_q_grp_mount,
        "dome"   => tr.sched_q_grp_dome,
        "cap"    => tr.sched_q_grp_cap,
        "camera" => tr.sched_q_grp_camera,
        "indi"   => tr.sched_q_grp_indi,
        _        => tr.sched_q_grp_other,
    }
}

fn choice_label(tr: &'static Translations, choice: &str) -> &'static str {
    match choice {
        "indi_set"   => tr.sched_q_step_indi_set,
        "indi_wait"  => tr.sched_q_step_indi_wait,
        "script_new" => tr.sched_q_step_script,
        "script_ext" => tr.sched_q_step_script_ext,
        id           => step_label(tr, id),
    }
}

fn choice_needs_device(choice: &str) -> bool {
    choice.starts_with("indi_") || template(choice).is_some_and(|t| t.needs_device)
}

/// A text field of `width`, outlined red while its value is invalid.
pub(super) fn input_cls(width: &str, ok: bool) -> String {
    format!("{INPUT} {width}{}", if ok { "" } else { " border-state-err" })
}

// ── Rows ─────────────────────────────────────────────────────────────────────

/// One step card. Every field lives in its own signal so typing doesn't touch
/// the row list — `rows` only changes on add/remove/reorder, and `<For>` keys
/// cards by `key`, so no input is rebuilt (and loses focus) mid-edit.
#[derive(Clone)]
struct StepRow {
    key: u32,
    kind: RowKind,
}

#[derive(Clone)]
enum RowKind {
    Template { id: &'static str, values: Vec<RwSignal<String>>, fail: RwSignal<u8> },
    /// `body` is `None` while the script is still being fetched.
    Managed { name: RwSignal<String>, body: RwSignal<Option<String>>, timeout: RwSignal<String> },
    External { path: RwSignal<String>, timeout: RwSignal<String> },
    Indi(IndiRow),
    Unknown { task: serde_json::Value, native: bool },
}

impl StepRow {
    fn new(key: u32, step: QueueStep) -> Self {
        let kind = match step {
            QueueStep::Template { id, values, on_missing_device } => RowKind::Template {
                id,
                values: values.into_iter().map(RwSignal::new).collect(),
                fail: RwSignal::new(on_missing_device),
            },
            QueueStep::Script { script: ScriptRef::Managed { name, body }, timeout } => RowKind::Managed {
                name: RwSignal::new(name),
                body: RwSignal::new((!body.is_empty()).then_some(body)),
                timeout: RwSignal::new(timeout),
            },
            QueueStep::Script { script: ScriptRef::External { path }, timeout } => RowKind::External {
                path: RwSignal::new(path),
                timeout: RwSignal::new(timeout),
            },
            QueueStep::Indi(s) => RowKind::Indi(IndiRow::new(s)),
            QueueStep::Unknown { task, native } => RowKind::Unknown { task, native },
        };
        Self { key, kind }
    }

    /// The step as it stands, or `None` while a script body is still loading.
    fn snapshot(&self) -> Option<QueueStep> {
        Some(match &self.kind {
            RowKind::Template { id, values, fail } => QueueStep::Template {
                id,
                values: values.iter().map(|v| v.get_untracked()).collect(),
                on_missing_device: fail.get_untracked(),
            },
            RowKind::Managed { name, body, timeout } => QueueStep::Script {
                script: ScriptRef::Managed { name: name.get_untracked().trim().to_string(), body: body.get_untracked()? },
                timeout: timeout.get_untracked(),
            },
            RowKind::External { path, timeout } => QueueStep::Script {
                script: ScriptRef::External { path: path.get_untracked() },
                timeout: timeout.get_untracked(),
            },
            RowKind::Indi(row) => QueueStep::Indi(row.snapshot()),
            RowKind::Unknown { task, native } => QueueStep::Unknown { task: task.clone(), native: *native },
        })
    }

    fn needs_device(&self) -> bool {
        match &self.kind {
            RowKind::Template { id, .. } => template(id).is_some_and(|t| t.needs_device),
            RowKind::Indi(_) => true,
            _ => false,
        }
    }

    /// Same rule as [`QueueStep::needs_native`].
    fn needs_native(&self) -> bool {
        matches!(&self.kind, RowKind::Indi(_) | RowKind::Unknown { native: true, .. })
    }
}

// ── Labels ───────────────────────────────────────────────────────────────────

pub(super) fn slot_title(tr: &'static Translations, slot: QueueSlot) -> &'static str {
    match slot {
        QueueSlot::PreStartup   => tr.sched_q_slot_pre_startup,
        QueueSlot::PostStartup  => tr.sched_q_slot_post_startup,
        QueueSlot::PreShutdown  => tr.sched_q_slot_pre_shutdown,
        QueueSlot::PostShutdown => tr.sched_q_slot_post_shutdown,
    }
}

fn preset_label(tr: &'static Translations, preset: Preset) -> &'static str {
    match preset {
        Preset::Empty    => tr.sched_q_preset_empty,
        Preset::Startup  => tr.sched_q_preset_startup,
        Preset::Shutdown => tr.sched_q_preset_shutdown,
    }
}

fn error_text(tr: &'static Translations, e: &QueueError) -> String {
    let at = |step: &usize, msg: &str| format!("#{} — {}", step + 1, msg);
    match e {
        QueueError::BadName      => tr.sched_q_err_name.to_string(),
        QueueError::ReservedName => tr.sched_q_err_reserved.to_string(),
        QueueError::NoSteps      => tr.sched_q_err_no_steps.to_string(),
        QueueError::DeviceStepInSlot { step }    => at(step, tr.sched_q_err_device_slot),
        QueueError::BadParam { step, param, min, max } => at(
            step,
            &format!("{} {} ({} … {})", param_label(tr, param), tr.sched_q_err_range, fmt_num(*min), fmt_num(*max)),
        ),
        QueueError::BadScriptName { step }       => at(step, tr.sched_q_err_script_name),
        QueueError::DuplicateScriptName { step } => at(step, tr.sched_q_err_script_dup),
        QueueError::MissingShebang { step }      => at(step, tr.sched_q_err_shebang),
        QueueError::BadScriptPath { step }       => at(step, tr.sched_q_err_script_path),
        QueueError::IndiIncomplete { step }      => at(step, tr.sched_q_err_indi_incomplete),
        QueueError::IndiBadValue { step }        => at(step, tr.sched_q_err_indi_value),
        QueueError::UnknownInQueue { step }      => at(step, tr.sched_q_err_unknown_queue),
    }
}

/// Put a `<select>` used as a menu back on its placeholder.
fn reset_select(ev: &leptos::ev::Event) {
    if let Some(el) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlSelectElement>().ok()) {
        el.set_value("");
    }
}

fn confirm(message: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.confirm_with_message(message).ok())
        .unwrap_or(false)
}

// ── Fields ───────────────────────────────────────────────────────────────────

/// Text rather than `type=number`: a number input reports `""` for a
/// half-typed `-`, which the reactive `prop:value` would then write back and
/// wipe. Range errors are flagged here and enforced by `validate`.
pub(super) fn number_field(lang: RwSignal<Lang>, p: &'static ParamSpec, value: RwSignal<String>) -> impl IntoView {
    let tr = move || t(lang.get());
    let out_of_range = move || {
        !value.with(|v| v.trim().parse::<f64>().is_ok_and(|n| n >= p.min && n <= p.max))
    };
    view! {
        <label class="flex items-center gap-sp-1">
            <span class=format!("{FIELD_LABEL} min-w-0")>{move || param_label(tr(), p.name)}</span>
            <input
                class=move || input_cls("w-[84px]", !out_of_range())
                inputmode={if p.min < 0.0 { "text" } else { "decimal" }}
                title=format!("{} … {}", fmt_num(p.min), fmt_num(p.max))
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            />
            <span class=UNIT>{p.unit}</span>
        </label>
    }
}

fn step_card(
    row: StepRow,
    rows: RwSignal<Vec<StepRow>>,
    lang: RwSignal<Lang>,
    scripts_dir: RwSignal<String>,
    source: IndiSource,
    // The queue is in KStars' queue format, which ignores "if no device".
    native: Signal<bool>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let key = row.key;
    let index = move || rows.with(|v| v.iter().position(|r| r.key == key).unwrap_or(0));
    let count = move || rows.with(Vec::len);
    let move_by = move |delta: isize| {
        rows.update(|v| {
            let Some(i) = v.iter().position(|r| r.key == key) else { return };
            let j = i as isize + delta;
            if j >= 0 && (j as usize) < v.len() {
                v.swap(i, j as usize);
            }
        })
    };

    let (label, body) = match row.kind {
        RowKind::Template { id, values, fail } => {
            let spec = template(id).expect("template rows are only built for known ids");
            let fields = spec
                .params
                .iter()
                .zip(values)
                .map(|(p, value)| number_field(lang, p, value))
                .collect::<Vec<_>>();
            // What KStars will do: the queue format always aborts.
            let shown = move || if native.get() { FAIL_ABORT } else { fail.get() };
            let fail_select = spec.needs_device.then(|| view! {
                <label class="flex items-center gap-sp-1">
                    <span class=format!("{FIELD_LABEL} min-w-0")>{move || tr().sched_q_if_no_device}</span>
                    <select
                        class=SELECT
                        prop:disabled=move || native.get()
                        on:change=move |ev| fail.set(event_target_value(&ev).parse().unwrap_or(FAIL_SKIP))
                    >
                        <option value=FAIL_SKIP.to_string() prop:selected=move || shown() == FAIL_SKIP>
                            {move || tr().sched_q_fail_skip}
                        </option>
                        <option value=FAIL_CONTINUE.to_string() prop:selected=move || shown() == FAIL_CONTINUE>
                            {move || tr().sched_q_fail_continue}
                        </option>
                        <option value=FAIL_ABORT.to_string() prop:selected=move || shown() == FAIL_ABORT>
                            {move || tr().sched_q_fail_abort}
                        </option>
                    </select>
                </label>
            });
            let label = Signal::derive(move || step_label(tr(), id).to_string());
            (label, view! { <div class=FIELDS>{fields}{fail_select}</div> }.into_any())
        }
        RowKind::Managed { name, body, timeout } => {
            let label = Signal::derive(move || tr().sched_q_step_script.to_string());
            let view = view! {
                <div class=FIELDS>
                    <label class="flex items-center gap-sp-1">
                        <span class=format!("{FIELD_LABEL} min-w-0")>{move || tr().sched_q_script_name}</span>
                        <input
                            class=move || input_cls("w-[200px]", name.with(|n| is_safe_name(n.trim())))
                            prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))
                        />
                        <span class=UNIT>".sh"</span>
                    </label>
                    {number_field(lang, &SCRIPT_TIMEOUT, timeout)}
                </div>
                <div class="text-text-faint text-xs font-mono break-all">
                    {move || managed_script_path(&scripts_dir.get(), name.get().trim())}
                </div>
                <textarea
                    class="input font-mono text-sm w-full h-auto min-h-[180px] py-2 resize-y leading-snug whitespace-pre"
                    spellcheck="false"
                    aria-label=move || tr().sched_q_script_body
                    placeholder=move || tr().sched_q_script_loading
                    prop:disabled=move || body.with(Option::is_none)
                    prop:value=move || body.get().unwrap_or_default()
                    on:input=move |ev| body.set(Some(event_target_value(&ev)))
                ></textarea>
                <select
                    class=format!("{SELECT} self-start max-md:w-full")
                    prop:disabled=move || body.with(Option::is_none)
                    on:change=move |ev| {
                        let key = event_target_value(&ev);
                        reset_select(&ev);
                        let Some(snippet) = SNIPPETS.iter().find(|s| s.key == key) else { return };
                        body.update(|b| {
                            if let Some(b) = b {
                                if !b.is_empty() && !b.ends_with('\n') {
                                    b.push('\n');
                                }
                                b.push_str(snippet.body);
                            }
                        });
                    }
                >
                    <option value="" selected>{move || tr().sched_q_snippet}</option>
                    {SNIPPETS.iter().map(|s| view! {
                        <option value=s.key>{move || snippet_label(tr(), s.key)}</option>
                    }).collect_view()}
                </select>
            }
            .into_any();
            (label, view)
        }
        RowKind::External { path, timeout } => {
            let label = Signal::derive(move || tr().sched_q_step_script_ext.to_string());
            let view = view! {
                <div class=FIELDS>
                    <span class=FIELD_LABEL>{move || tr().sched_q_script_path}</span>
                    <input
                        class=move || input_cls("flex-1 min-w-[200px]", path.with(|p| p.trim().starts_with('/')))
                        placeholder="/home/astronaut/bin/open_roof.sh"
                        prop:value=move || path.get()
                        on:input=move |ev| path.set(event_target_value(&ev))
                    />
                    {number_field(lang, &SCRIPT_TIMEOUT, timeout)}
                </div>
            }
            .into_any();
            (label, view)
        }
        RowKind::Indi(indi) => {
            let label = Signal::derive(move || match indi.op.get() {
                IndiOp::Set  => tr().sched_q_step_indi_set.to_string(),
                IndiOp::Wait => tr().sched_q_step_indi_wait.to_string(),
            });
            (label, indi_body(indi, key, lang, source).into_any())
        }
        RowKind::Unknown { task, .. } => {
            let id = match task["template_id"].as_str() {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => task["name"].as_str().unwrap_or("?").to_string(),
            };
            let label = Signal::derive(move || tr().sched_q_step_unknown.to_string());
            (label, view! { <div class="text-text-muted text-xs font-mono break-all">{id}</div> }.into_any())
        }
    };

    view! {
        <div class="border border-border-base rounded-md bg-bg-elev-1 p-sp-3 flex flex-col gap-sp-2">
            <div class="flex items-center gap-sp-2">
                <span class="text-text-faint text-xs font-mono">{move || format!("#{}", index() + 1)}</span>
                <span class="text-text-blue text-sm font-semibold">{label}</span>
                <span class="flex-1"></span>
                <button
                    class="btn-icon"
                    title=move || tr().sched_q_move_up
                    prop:disabled=move || index() == 0
                    on:click=move |_| move_by(-1)
                >"↑"</button>
                <button
                    class="btn-icon"
                    title=move || tr().sched_q_move_down
                    prop:disabled={move || index() + 1 >= count()}
                    on:click=move |_| move_by(1)
                >"↓"</button>
                <button
                    class="btn-icon"
                    title=move || tr().sched_q_remove_step
                    on:click=move |_| rows.update(|v| v.retain(|r| r.key != key))
                >"✕"</button>
            </div>
            {body}
        </div>
    }
}

// ── Editor ───────────────────────────────────────────────────────────────────

#[component]
pub fn SchedulerQueueEditor(
    #[prop(into)] lang: RwSignal<Lang>,
    #[prop(into)] send: SendCmd,
    /// Named `queue_slot`: `slot` is reserved by `view!` for slot components.
    queue_slot: QueueSlot,
    list: RwSignal<Option<QueueList>>,
    /// The slot's path field and its procedure's enable toggle, both updated
    /// on save so the settings overlay reflects what KStars now has.
    path: RwSignal<String>,
    enabled: RwSignal<bool>,
    /// Suggestions for custom INDI steps.
    devices: RwSignal<Vec<DeviceInfo>>,
    indi_properties: RwSignal<HashMap<String, Vec<IndiProperty>>>,
    on_close: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let slot = queue_slot;
    let source = IndiSource {
        devices,
        props: indi_properties,
        send: Arc::clone(&send),
        asked: StoredValue::new(Vec::new()),
    };

    let name        = RwSignal::new(slot.default_name().to_string());
    let title       = RwSignal::new(String::new());
    // `(name, path)` of the managed queue being edited; `None` for a new one.
    let existing    = RwSignal::new(Option::<(String, String)>::None);
    let rows        = RwSignal::new(Vec::<StepRow>::new());
    let preset      = RwSignal::new(slot.default_preset());
    let scripts_dir = RwSignal::new(String::new());
    let collections_dir = RwSignal::new(String::new());
    // Slot currently points at a file this editor doesn't manage.
    let foreign     = RwSignal::new(Option::<String>::None);
    let loading     = RwSignal::new(true);
    let busy        = RwSignal::new(false);
    let error       = RwSignal::new(Option::<String>::None);
    // Script names the loaded queue already owns — re-saving those is expected;
    // writing over any *other* script on disk needs a confirmation.
    let own_scripts = StoredValue::new(Vec::<String>::new());

    // Rows are built from event handlers and async loads, where there is no
    // current owner — create their signals under the editor's own, so they're
    // disposed with it.
    let owner = StoredValue::new(Owner::current());
    let next_key = StoredValue::new(0u32);
    let new_rows = move |steps: Vec<QueueStep>| -> Vec<StepRow> {
        let build = move || {
            steps
                .into_iter()
                .map(|step| {
                    let key = next_key.get_value();
                    next_key.set_value(key + 1);
                    StepRow::new(key, step)
                })
                .collect::<Vec<_>>()
        };
        match owner.get_value() {
            Some(o) => o.with(build),
            None => build(),
        }
    };

    // ── Initial load ────────────────────────────────────────────────────────
    //
    // Always re-list: the settings overlay's copy may be stale or still in
    // flight. Then open the managed queue the slot points at, if any.
    {
        let current = path.get_untracked().trim().to_string();
        wasm_bindgen_futures::spawn_local(async move {
            let fresh = match queue_api::fetch_list().await {
                Ok(l) => l,
                Err(e) => {
                    error.set(Some(e));
                    rows.set(new_rows(slot.default_preset().steps()));
                    loading.set(false);
                    return;
                }
            };
            scripts_dir.set(fresh.scripts_dir.clone());
            collections_dir.set(fresh.collections_dir.clone());
            let entry = fresh
                .queues
                .iter()
                .find(|q| !current.is_empty() && q.path == current && q.tasks.is_some())
                .cloned();
            list.set(Some(fresh.clone()));

            let Some(entry) = entry else {
                if !current.is_empty() {
                    foreign.set(Some(current));
                }
                rows.set(new_rows(slot.default_preset().steps()));
                loading.set(false);
                return;
            };

            let loaded = queue_api::fetch_queue(&entry.name)
                .await
                .and_then(|doc| from_document(&doc, &fresh.scripts_dir));
            let (queue_title, steps) = match loaded {
                Ok(v) => v,
                Err(e) => {
                    error.set(Some(format!("{}.json: {e}", entry.name)));
                    rows.set(new_rows(slot.default_preset().steps()));
                    loading.set(false);
                    return;
                }
            };
            name.set(entry.name.clone());
            title.set(queue_title);
            existing.set(Some((entry.name, entry.path)));
            let built = new_rows(steps);
            // Collected before any await: the editor may be closed meanwhile,
            // and a disposed signal panics on read (a `set` is a no-op).
            let pending: Vec<(String, RwSignal<Option<String>>)> = built
                .iter()
                .filter_map(|r| match &r.kind {
                    RowKind::Managed { name, body, .. } if body.get_untracked().is_none() => {
                        Some((name.get_untracked(), *body))
                    }
                    _ => None,
                })
                .collect();
            own_scripts.set_value(pending.iter().map(|(n, _)| n.clone()).collect());
            rows.set(built);
            loading.set(false);

            for (script, body) in pending {
                match queue_api::fetch_script(&script).await {
                    Ok(text) => body.set(Some(text)),
                    Err(e) => {
                        // Missing or unreadable — start it over; saving recreates it.
                        body.set(Some(NEW_SCRIPT_BODY.to_string()));
                        error.set(Some(format!("{script}.sh: {e}")));
                    }
                }
            }
        });
    }

    let shows_device_warning = move || !slot.allows_devices() && rows.with(|v| v.iter().any(StepRow::needs_device));
    let native = Signal::derive(move || rows.with(|v| v.iter().any(StepRow::needs_native)));

    // ── Add a step ──────────────────────────────────────────────────────────
    let fresh_script_name = move || {
        let base = {
            let n = name.get_untracked();
            let n = n.trim();
            let n = if is_safe_name(n) { n } else { "script" };
            n.chars().take(56).collect::<String>()
        };
        // Avoid every script already on disk too: saving always overwrites,
        // and another queue may own `<base>_1.sh`.
        let mut taken: Vec<String> = rows.with_untracked(|v| {
            v.iter()
                .filter_map(|r| match &r.kind {
                    RowKind::Managed { name, .. } => Some(name.get_untracked()),
                    _ => None,
                })
                .collect()
        });
        list.with_untracked(|l| {
            if let Some(l) = l {
                taken.extend(l.scripts.iter().map(|s| s.name.clone()));
            }
        });
        (1..)
            .map(|i| format!("{base}_{i}"))
            .find(|c| !taken.contains(c))
            .unwrap_or_else(|| format!("{base}_x"))
    };
    let adding = RwSignal::new(false);
    let on_add = move |choice: &str| {
        adding.set(false);
        let timeout = fmt_num(SCRIPT_TIMEOUT.default);
        let step = match choice {
            "indi_set" => QueueStep::Indi(IndiStep::new(IndiOp::Set)),
            "indi_wait" => QueueStep::Indi(IndiStep::new(IndiOp::Wait)),
            "script_new" => QueueStep::Script {
                script: ScriptRef::Managed { name: fresh_script_name(), body: NEW_SCRIPT_BODY.to_string() },
                timeout,
            },
            "script_ext" => QueueStep::Script { script: ScriptRef::External { path: String::new() }, timeout },
            id => match QueueStep::new_template(id) {
                Some(step) => step,
                None => return,
            },
        };
        let mut added = new_rows(vec![step]);
        rows.update(|v| v.append(&mut added));
    };

    // ── Save & assign ───────────────────────────────────────────────────────
    let send_save = Arc::clone(&send);
    let close_save = Arc::clone(&on_close);
    let on_save = move |_| {
        if busy.get_untracked() {
            return;
        }
        let tr = t(lang.get_untracked());
        let queue_name = name.get_untracked().trim().to_string();
        let Some(steps) = rows.with_untracked(|v| v.iter().map(StepRow::snapshot).collect::<Option<Vec<_>>>()) else {
            error.set(Some(tr.sched_q_err_loading.to_string()));
            return;
        };
        if let Err(e) = validate(slot, &queue_name, &steps) {
            error.set(Some(error_text(tr, &e)));
            return;
        }
        let dir = scripts_dir.get_untracked();
        if dir.is_empty() {
            error.set(Some(tr.sched_q_err_no_server.to_string()));
            return;
        }
        let queue_title = {
            let t = title.get_untracked();
            if t.trim().is_empty() { queue_name.clone() } else { t.trim().to_string() }
        };
        let doc = to_document(&queue_title, &steps, &dir);
        let scripts: Vec<(String, String)> = steps
            .iter()
            .filter_map(|s| match s {
                QueueStep::Script { script: ScriptRef::Managed { name, body }, .. } => Some((name.clone(), body.clone())),
                _ => None,
            })
            .collect();
        let mut overwrite = existing.with_untracked(|e| e.as_ref().is_some_and(|(n, _)| *n == queue_name));

        // Ask about name collisions *before* writing anything — the scripts go
        // out first, so a late 409 on the queue would already have replaced
        // another queue's script. (The server's 409 still covers a race.)
        let (queue_taken, foreign_scripts) = list.with_untracked(|l| {
            let Some(l) = l else { return (false, Vec::new()) };
            let own = own_scripts.get_value();
            let queue_taken = !overwrite && l.queues.iter().any(|q| q.name == queue_name);
            let foreign_scripts: Vec<String> = scripts
                .iter()
                .map(|(n, _)| n.clone())
                .filter(|n| !own.contains(n) && l.scripts.iter().any(|s| s.name == *n))
                .collect();
            (queue_taken, foreign_scripts)
        });
        if queue_taken {
            if !confirm(&format!("{queue_name}.json — {}", tr.sched_q_confirm_overwrite)) {
                return;
            }
            overwrite = true;
        }
        if !foreign_scripts.is_empty() {
            let files = foreign_scripts.iter().map(|n| format!("{n}.sh")).collect::<Vec<_>>().join(", ");
            if !confirm(&format!("{files} — {}", tr.sched_q_confirm_overwrite_script)) {
                return;
            }
        }

        let send = Arc::clone(&send_save);
        let on_close = Arc::clone(&close_save);
        busy.set(true);
        error.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            // Scripts first: the collection references them by path, and
            // KStars may load it the moment the slot changes.
            for (script, body) in &scripts {
                if let Err(e) = queue_api::save_script(script, body).await {
                    error.set(Some(format!("{script}.sh: {e}")));
                    busy.set(false);
                    return;
                }
            }
            let saved = match queue_api::save_queue(&queue_name, &doc, overwrite).await {
                Err(SaveErr::Exists) => {
                    if !confirm(&format!("{queue_name}.json — {}", tr.sched_q_confirm_overwrite)) {
                        busy.set(false);
                        return;
                    }
                    queue_api::save_queue(&queue_name, &doc, true).await
                }
                other => other,
            };
            let abs = match saved {
                Ok(abs) => abs,
                Err(SaveErr::Exists) => {
                    error.set(Some(format!("{queue_name}.json — {}", tr.sched_q_confirm_overwrite)));
                    busy.set(false);
                    return;
                }
                Err(SaveErr::Failed(e)) => {
                    error.set(Some(e));
                    busy.set(false);
                    return;
                }
            };

            path.set(abs.clone());
            enabled.set(true);
            let mut settings = serde_json::Map::new();
            settings.insert(slot.setting_key().to_string(), abs.into());
            settings.insert(slot.enable_key().to_string(), true.into());
            send_cmd(&send, "scheduler_set_all_settings", serde_json::Value::Object(settings));

            if let Ok(l) = queue_api::fetch_list().await {
                list.set(Some(l));
            }
            busy.set(false);
            on_close();
        });
    };

    // ── Delete ──────────────────────────────────────────────────────────────
    let send_delete = Arc::clone(&send);
    let close_delete = Arc::clone(&on_close);
    let on_delete = move |_| {
        let Some((queue_name, queue_path)) = existing.get_untracked() else { return };
        let tr = t(lang.get_untracked());
        if busy.get_untracked() || !confirm(&format!("{queue_name}.json — {}", tr.sched_q_confirm_delete)) {
            return;
        }
        let assigned = path.get_untracked().trim() == queue_path;
        let send = Arc::clone(&send_delete);
        let on_close = Arc::clone(&close_delete);
        busy.set(true);
        error.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = queue_api::delete_queue(&queue_name).await {
                error.set(Some(e));
                busy.set(false);
                return;
            }
            // Don't leave the slot pointing at a file that no longer exists —
            // KStars would fail the whole procedure on it.
            if assigned {
                path.set(String::new());
                let mut settings = serde_json::Map::new();
                settings.insert(slot.setting_key().to_string(), "".into());
                send_cmd(&send, "scheduler_set_all_settings", serde_json::Value::Object(settings));
            }
            if let Ok(l) = queue_api::fetch_list().await {
                list.set(Some(l));
            }
            busy.set(false);
            on_close();
        });
    };

    let close = Arc::clone(&on_close);
    let add_groups = move || {
        ADD_GROUPS
            .iter()
            .filter(|(_, choices)| slot.allows_devices() || choices.iter().any(|c| !choice_needs_device(c)))
            .map(|(group, choices)| view! {
                <div class="flex flex-col gap-1.5">
                    <span class=CARD_TITLE>{move || group_label(tr(), group)}</span>
                    <div class="flex flex-wrap gap-1.5">
                        {choices
                            .iter()
                            .filter(|c| slot.allows_devices() || !choice_needs_device(c))
                            .map(|choice| view! {
                                <button type="button" class=CHIP on:click=move |_| on_add(choice)>
                                    {move || choice_label(tr(), choice)}
                                </button>
                            })
                            .collect_view()}
                    </div>
                </div>
            })
            .collect_view()
    };

    view! {
        <div class="flex-1 min-h-0 flex flex-col">
            <div class="shrink-0 flex items-center gap-2 min-h-[44px] px-2 md:px-4 border-b border-border-base">
                <button class="btn-icon shrink-0" title=move || tr().sched_q_back on:click=move |_| close()>
                    <span class="md:hidden">"\u{2039}"</span>
                    <span class="max-md:hidden">"\u{2716}"</span>
                </button>
                <span class="flex-1 min-w-0 truncate font-semibold text-text-blue">{move || slot_title(tr(), slot)}</span>
            </div>

            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:px-4 flex flex-col gap-3">
                <Show
                    when=move || !loading.get()
                    fallback=move || view! { <div class="text-text-muted text-sm">{move || tr().sched_q_script_loading}</div> }
                >
                    {move || foreign.get().map(|p| view! {
                        <div class="text-text-muted text-xs">
                            {move || tr().sched_q_foreign_note}
                            " "
                            <span class="font-mono break-all">{p}</span>
                        </div>
                    })}

                    <div class=FIELDS>
                        <label class="flex items-center gap-sp-1">
                            <span class=FIELD_LABEL>{move || tr().sched_q_name}</span>
                            <input
                                class=move || input_cls("w-[220px] max-md:flex-1", name.with(|n| is_safe_name(n.trim())))
                                prop:value=move || name.get()
                                on:input=move |ev| name.set(event_target_value(&ev))
                            />
                            <span class=UNIT>".json"</span>
                        </label>
                        <label class="flex items-center gap-sp-1 flex-1 min-w-[200px]">
                            <span class=FIELD_LABEL>{move || tr().sched_q_title}</span>
                            <input
                                class=format!("{INPUT} flex-1 min-w-0")
                                placeholder=move || name.get()
                                prop:value=move || title.get()
                                on:input=move |ev| title.set(event_target_value(&ev))
                            />
                        </label>
                    </div>
                    <div class="text-text-faint text-xs font-mono break-all -mt-sp-2">
                        {move || format!(
                            "→ {}/{}.json",
                            collections_dir.get().trim_end_matches('/'),
                            name.get().trim(),
                        )}
                    </div>

                    <Show when=move || existing.with(Option::is_none) && slot.allows_devices()>
                        <label class=FIELDS>
                            <span class=FIELD_LABEL>{move || tr().sched_q_start_from}</span>
                            <select
                                class=format!("{SELECT} max-md:flex-1 min-w-0")
                                on:change=move |ev| {
                                    let p = Preset::from_key(&event_target_value(&ev));
                                    preset.set(p);
                                    rows.set(new_rows(p.steps()));
                                }
                            >
                                {[Preset::Empty, Preset::Startup, Preset::Shutdown]
                                    .into_iter()
                                    .map(|p| view! {
                                        <option value=p.key() prop:selected=move || preset.get() == p>
                                            {move || preset_label(tr(), p)}
                                        </option>
                                    })
                                    .collect::<Vec<_>>()}
                            </select>
                        </label>
                    </Show>

                    <Show when=move || !slot.allows_devices()>
                        <div class="text-text-muted text-xs">{move || tr().sched_q_devices_note}</div>
                    </Show>
                    <Show when=shows_device_warning>
                        <div class="text-state-warn text-sm">{move || tr().sched_q_device_warning}</div>
                    </Show>
                    <Show when=move || native.get()>
                        <div class="text-text-muted text-xs">{move || tr().sched_q_native_note}</div>
                    </Show>

                    <div class="flex flex-col gap-2">
                        <For
                            each=move || rows.get()
                            key=|r| r.key
                            children={
                                let source = source.clone();
                                move |row: StepRow| step_card(row, rows, lang, scripts_dir, source.clone(), native)
                            }
                        />
                    </div>
                    <Show when=move || rows.with(Vec::is_empty)>
                        <div class="text-text-faint text-sm">{move || tr().sched_q_no_steps}</div>
                    </Show>

                    <Show when=move || adding.get()>
                        <div class="panel p-3 flex flex-col gap-3">{add_groups}</div>
                    </Show>
                    <button
                        type="button"
                        class=move || if adding.get() { "btn btn-ghost self-start" } else { "btn btn-primary self-start" }
                        aria-expanded=move || adding.get().to_string()
                        on:click=move |_| adding.update(|a| *a = !*a)
                    >
                        {move || if adding.get() { tr().info_close } else { tr().sched_q_add_step }}
                    </button>
                </Show>
            </div>

            <div class=format!("{FOOTER} md:px-4")>
                <div class="flex-1 min-w-0">
                    {move || error.get().map(|e| view! { <div class="text-state-err text-sm">{e}</div> })}
                </div>
                <Show when=move || existing.with(Option::is_some)>
                    <button
                        class="btn btn-danger h-11 shrink-0"
                        prop:disabled=move || busy.get()
                        on:click=on_delete.clone()
                    >{move || tr().sched_q_delete}</button>
                </Show>
                <button
                    class="btn btn-primary h-11 px-5 shrink-0"
                    prop:disabled=move || busy.get() || loading.get()
                    on:click=on_save
                >
                    {move || if busy.get() { tr().sched_q_saving } else { tr().sched_q_save_assign }}
                </button>
            </div>
        </div>
    }
}
