//! Scheduler: the startup/shutdown queue editor overlay.
//!
//! Opens for one [`QueueSlot`]. It loads the managed collection the slot
//! already points at — or starts from the slot's stock preset — and lets the
//! user add, reorder and remove steps, shell scripts edited inline. Saving
//! writes the scripts and the collection through junos-server
//! (`/api/taskqueue/*`), then points the slot at the collection with
//! `scheduler_set_all_settings`.

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::queue_api::{self, QueueList, SaveErr};
use super::queue_model::{
    fmt_num, from_collection, is_safe_name, managed_script_path, template, to_collection, validate,
    ParamSpec, Preset, QueueError, QueueSlot, QueueStep, ScriptRef, FAIL_ABORT, FAIL_CONTINUE,
    FAIL_SKIP, NEW_SCRIPT_BODY, SCRIPT_TIMEOUT, TEMPLATES,
};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

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
    Unknown(serde_json::Value),
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
            QueueStep::Unknown(task) => RowKind::Unknown(task),
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
            RowKind::Unknown(task) => QueueStep::Unknown(task.clone()),
        })
    }

    fn needs_device(&self) -> bool {
        matches!(&self.kind, RowKind::Template { id, .. } if template(id).is_some_and(|t| t.needs_device))
    }
}

// ── Labels ───────────────────────────────────────────────────────────────────

fn step_label(tr: &'static Translations, id: &str) -> &'static str {
    match id {
        "dome_unpark"         => tr.sched_q_step_dome_unpark,
        "dome_park"           => tr.sched_q_step_dome_park,
        "mount_unpark"        => tr.sched_q_step_mount_unpark,
        "mount_park"          => tr.sched_q_step_mount_park,
        "dustcap_unpark"      => tr.sched_q_step_dustcap_unpark,
        "dustcap_park"        => tr.sched_q_step_dustcap_park,
        "camera_cool"         => tr.sched_q_step_camera_cool,
        "camera_warm"         => tr.sched_q_step_camera_warm,
        "camera_warm_passive" => tr.sched_q_step_camera_warm_passive,
        "delay"               => tr.sched_q_step_delay,
        _                     => tr.sched_q_step_unknown,
    }
}

fn param_label(tr: &'static Translations, name: &str) -> &'static str {
    match name {
        "wait_timeout"       => tr.sched_q_p_wait_timeout,
        "target_temperature" => tr.sched_q_p_target_temperature,
        "tolerance"          => tr.sched_q_p_tolerance,
        "ramp_slope"         => tr.sched_q_p_ramp_slope,
        "ramp_threshold"     => tr.sched_q_p_ramp_threshold,
        "max_wait_time"      => tr.sched_q_p_max_wait_time,
        "delay_seconds"      => tr.sched_q_p_delay_seconds,
        _                    => tr.sched_q_p_timeout,
    }
}

fn slot_title(tr: &'static Translations, slot: QueueSlot) -> &'static str {
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
fn number_field(lang: RwSignal<Lang>, p: &'static ParamSpec, value: RwSignal<String>) -> impl IntoView {
    let tr = move || t(lang.get());
    let out_of_range = move || {
        !value.with(|v| v.trim().parse::<f64>().is_ok_and(|n| n >= p.min && n <= p.max))
    };
    view! {
        <label class="flex items-center gap-sp-1">
            <span class="sched-field-label min-w-0">{move || param_label(tr(), p.name)}</span>
            <input
                class=move || if out_of_range() { "sched-input w-[84px] border-state-err" } else { "sched-input w-[84px]" }
                inputmode={if p.min < 0.0 { "text" } else { "decimal" }}
                title=format!("{} … {}", fmt_num(p.min), fmt_num(p.max))
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            />
            <span class="sched-field-unit">{p.unit}</span>
        </label>
    }
}

fn step_card(
    row: StepRow,
    rows: RwSignal<Vec<StepRow>>,
    lang: RwSignal<Lang>,
    scripts_dir: RwSignal<String>,
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
            let fail_select = spec.needs_device.then(|| view! {
                <label class="flex items-center gap-sp-1">
                    <span class="sched-field-label min-w-0">{move || tr().sched_q_if_no_device}</span>
                    <select
                        class="sched-select"
                        on:change=move |ev| fail.set(event_target_value(&ev).parse().unwrap_or(FAIL_SKIP))
                    >
                        <option value=FAIL_SKIP.to_string() prop:selected=move || fail.get() == FAIL_SKIP>
                            {move || tr().sched_q_fail_skip}
                        </option>
                        <option value=FAIL_CONTINUE.to_string() prop:selected=move || fail.get() == FAIL_CONTINUE>
                            {move || tr().sched_q_fail_continue}
                        </option>
                        <option value=FAIL_ABORT.to_string() prop:selected=move || fail.get() == FAIL_ABORT>
                            {move || tr().sched_q_fail_abort}
                        </option>
                    </select>
                </label>
            });
            let label = Signal::derive(move || step_label(tr(), id).to_string());
            (label, view! { <div class="sched-field-row">{fields}{fail_select}</div> }.into_any())
        }
        RowKind::Managed { name, body, timeout } => {
            let label = Signal::derive(move || tr().sched_q_step_script.to_string());
            let view = view! {
                <div class="sched-field-row">
                    <label class="flex items-center gap-sp-1">
                        <span class="sched-field-label min-w-0">{move || tr().sched_q_script_name}</span>
                        <input
                            class=move || if name.with(|n| is_safe_name(n.trim())) {
                                "sched-input w-[200px]"
                            } else {
                                "sched-input w-[200px] border-state-err"
                            }
                            prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))
                        />
                        <span class="sched-field-unit">".sh"</span>
                    </label>
                    {number_field(lang, &SCRIPT_TIMEOUT, timeout)}
                </div>
                <div class="text-text-faint text-xs font-mono break-all">
                    {move || managed_script_path(&scripts_dir.get(), name.get().trim())}
                </div>
                <textarea
                    class="sched-input w-full min-h-[180px] resize-y leading-snug whitespace-pre"
                    spellcheck="false"
                    aria-label=move || tr().sched_q_script_body
                    placeholder=move || tr().sched_q_script_loading
                    prop:disabled=move || body.with(Option::is_none)
                    prop:value=move || body.get().unwrap_or_default()
                    on:input=move |ev| body.set(Some(event_target_value(&ev)))
                ></textarea>
            }
            .into_any();
            (label, view)
        }
        RowKind::External { path, timeout } => {
            let label = Signal::derive(move || tr().sched_q_step_script_ext.to_string());
            let view = view! {
                <div class="sched-field-row">
                    <span class="sched-field-label">{move || tr().sched_q_script_path}</span>
                    <input
                        class=move || if path.with(|p| p.trim().starts_with('/')) {
                            "sched-input sched-input-path"
                        } else {
                            "sched-input sched-input-path border-state-err"
                        }
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
        RowKind::Unknown(task) => {
            let id = task["template_id"].as_str().unwrap_or("?").to_string();
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
                    class="sched-btn-icon"
                    title=move || tr().sched_q_move_up
                    prop:disabled=move || index() == 0
                    on:click=move |_| move_by(-1)
                >"↑"</button>
                <button
                    class="sched-btn-icon"
                    title=move || tr().sched_q_move_down
                    prop:disabled={move || index() + 1 >= count()}
                    on:click=move |_| move_by(1)
                >"↓"</button>
                <button
                    class="sched-btn-icon"
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
    on_close: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let slot = queue_slot;

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
                .and_then(|doc| from_collection(&doc, &fresh.scripts_dir));
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
    let on_add = move |ev: leptos::ev::Event| {
        let choice = event_target_value(&ev);
        if let Some(el) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlSelectElement>().ok()) {
            el.set_value("");
        }
        let timeout = fmt_num(SCRIPT_TIMEOUT.default);
        let step = match choice.as_str() {
            "" => return,
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
        let doc = to_collection(&queue_title, &steps, &dir);
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

    let close_btn = Arc::clone(&on_close);

    view! {
        <div class="fixed inset-0 z-[60] bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2">
            <div class="w-full max-w-[860px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col">
                <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                    <h2 class="text-text-blue text-sm uppercase tracking-[0.08em] m-0">
                        {move || slot_title(tr(), slot)}
                    </h2>
                    <button class="btn btn-ghost" on:click=move |_| close_btn()>
                        {move || tr().imaging_close}
                    </button>
                </div>

                <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 flex flex-col gap-sp-3">
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

                        <div class="sched-field-row">
                            <label class="flex items-center gap-sp-1">
                                <span class="sched-field-label">{move || tr().sched_q_name}</span>
                                <input
                                    class=move || if name.with(|n| is_safe_name(n.trim())) {
                                        "sched-input w-[220px]"
                                    } else {
                                        "sched-input w-[220px] border-state-err"
                                    }
                                    prop:value=move || name.get()
                                    on:input=move |ev| name.set(event_target_value(&ev))
                                />
                                <span class="sched-field-unit">".json"</span>
                            </label>
                            <label class="flex items-center gap-sp-1 flex-1 min-w-[200px]">
                                <span class="sched-field-label">{move || tr().sched_q_title}</span>
                                <input
                                    class="sched-input flex-1"
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
                            <label class="sched-field-row">
                                <span class="sched-field-label">{move || tr().sched_q_start_from}</span>
                                <select
                                    class="sched-select"
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

                        <div class="flex flex-col gap-sp-2">
                            <For
                                each=move || rows.get()
                                key=|r| r.key
                                children=move |row: StepRow| step_card(row, rows, lang, scripts_dir)
                            />
                        </div>
                        <Show when=move || rows.with(Vec::is_empty)>
                            <div class="text-text-faint text-sm">{move || tr().sched_q_no_steps}</div>
                        </Show>

                        <select class="sched-select self-start" on:change=on_add>
                            <option value="" selected>{move || tr().sched_q_add_step}</option>
                            {TEMPLATES
                                .iter()
                                .filter(|spec| slot.allows_devices() || !spec.needs_device)
                                .map(|spec| view! {
                                    <option value=spec.id>{move || step_label(tr(), spec.id)}</option>
                                })
                                .collect::<Vec<_>>()}
                            <option value="script_new">{move || tr().sched_q_step_script}</option>
                            <option value="script_ext">{move || tr().sched_q_step_script_ext}</option>
                        </select>
                    </Show>
                </div>

                <div class="flex items-center flex-wrap gap-sp-3 py-sp-3 px-sp-4 border-t border-border-base bg-[rgba(10,12,20,0.8)]">
                    <div class="flex-1 min-w-[200px]">
                        {move || error.get().map(|e| view! { <div class="sched-form-error">{e}</div> })}
                    </div>
                    <Show when=move || existing.with(Option::is_some)>
                        <button
                            class="sched-btn-clear"
                            prop:disabled=move || busy.get()
                            on:click=on_delete.clone()
                        >{move || tr().sched_q_delete}</button>
                    </Show>
                    <button
                        class="sched-btn-apply"
                        prop:disabled=move || busy.get() || loading.get()
                        on:click=on_save
                    >
                        {move || if busy.get() { tr().sched_q_saving } else { tr().sched_q_save_assign }}
                    </button>
                </div>
            </div>
        </div>
    }
}
