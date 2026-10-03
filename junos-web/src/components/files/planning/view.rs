//! Files › Planning: a planning file, formatted — a schedule's jobs, a
//! sequence's frames, a queue's tasks — above its source text.

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE};
use crate::components::frame_type::frame_type_visual;
use crate::components::scheduler::{param_label, step_label};
use crate::components::sequence_editor::fmt_duration;
use crate::components::sky::{fmt_dec, fmt_ra};
use crate::i18n::Translations;

use super::super::utils::{name_of, DASH};
use super::parse::{
    parse, Condition, Kind, Parsed, Procedure, Queue, QueueTask, Schedule, ScheduleJob, SeqRow, Sequence,
};

/// A static tag (constraint, step, filter).
const TAG: &str = "inline-flex items-center h-6 px-2 rounded-full border border-border-base text-xs text-text-dim";

/// Label left, value right; the full value on hover.
pub(crate) fn kv(label: &'static str, value: String) -> impl IntoView {
    let title = value.clone();
    view! {
        <div class="flex items-baseline justify-between gap-3 text-sm">
            <span class="shrink-0 text-text-muted">{label}</span>
            <span class="min-w-0 truncate text-right font-mono text-text-blue-bright" title=title>{value}</span>
        </div>
    }
}

fn num(v: f64) -> String {
    if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.1}") }
}

/// `2026-10-03T22:00:00` → `2026-10-03 22:00`.
fn iso_time(s: &str) -> String {
    let s = s.replacen('T', " ", 1);
    match s.get(..16) {
        Some(head) if s.len() > 16 => head.to_string(),
        _ => s,
    }
}

fn cond_label(c: &Condition, tr: &'static Translations) -> String {
    match c.name.as_str() {
        "" => DASH.to_string(),
        "ASAP" => tr.sched_cond_asap.to_string(),
        "At" => format!("{} {}", tr.plan_at, iso_time(&c.value)),
        "Sequence" => tr.sched_cond_seq.to_string(),
        "Repeat" => format!("{} \u{00d7}{}", tr.plan_repeat, c.value),
        "Loop" => tr.sched_cond_loop.to_string(),
        other if c.value.is_empty() => other.to_string(),
        other => format!("{other} ({})", c.value),
    }
}

fn step_name(step: &str, tr: &'static Translations) -> String {
    match step {
        "Track" => tr.sched_step_track,
        "Focus" => tr.sched_step_focus,
        "Align" => tr.sched_step_align,
        "Guide" => tr.sched_step_guide,
        other => other,
    }
    .to_string()
}

fn frame_label(frame_type: &str, tr: &'static Translations) -> String {
    match frame_type {
        "Light" => tr.frame_light,
        "Dark" => tr.frame_dark,
        "Bias" => tr.frame_bias,
        "Flat" => tr.frame_flat,
        "" => DASH,
        other => other,
    }
    .to_string()
}

fn tags(items: Vec<String>) -> Option<impl IntoView> {
    (!items.is_empty()).then(|| view! {
        <div class="flex flex-wrap gap-1.5">
            {items.into_iter().map(|t| view! { <span class=TAG>{t}</span> }).collect_view()}
        </div>
    })
}

// ── Schedule ─────────────────────────────────────────────────────────────────

fn job_card(j: ScheduleJob, tr: &'static Translations) -> impl IntoView {
    let title = if j.lead { j.name.clone() } else { tr.plan_follower.to_string() };
    let coords = j.ra_h.zip(j.dec_deg).map(|(ra, dec)| format!("{}  {}", fmt_ra(ra * 15.0), fmt_dec(dec)));
    let mut constraints = Vec::new();
    if let Some(v) = j.min_alt {
        constraints.push(format!("{} {}\u{00b0}", tr.sched_min_alt, num(v)));
    }
    if let Some(v) = j.moon_sep {
        constraints.push(format!("{} {}\u{00b0}", tr.sched_moon_sep, num(v)));
    }
    if let Some(v) = j.moon_max_alt {
        constraints.push(format!("{} {}\u{00b0}", tr.sched_moon_max_alt, num(v)));
    }
    if j.twilight {
        constraints.push(tr.sched_twilight.to_string());
    }
    if j.horizon {
        constraints.push(tr.sched_horizon.to_string());
    }
    let steps: Vec<String> = j.steps.iter().map(|s| step_name(s, tr)).collect();
    let sequence = j.sequence.clone();
    let start = j.lead.then(|| cond_label(&j.start, tr));
    let hover = title.clone();

    view! {
        <div class=CARD>
            <div class="flex items-baseline gap-2 min-w-0">
                <span class="min-w-0 truncate font-semibold text-text-blue-bright" title=hover>{title}</span>
                {(!j.group.is_empty()).then(|| view! {
                    <span class="shrink-0 text-xs text-text-faint">{j.group.clone()}</span>
                })}
            </div>
            {coords.map(|c| view! { <span class="font-mono text-xs text-text-muted">{c}</span> })}
            <div class="flex items-baseline justify-between gap-3 text-sm">
                <span class="shrink-0 text-text-muted">{tr.sched_seq_label}</span>
                <span class="min-w-0 truncate text-right font-mono text-text-blue-bright" title=sequence.clone()>
                    {if sequence.is_empty() { DASH.to_string() } else { name_of(&sequence).to_string() }}
                </span>
            </div>
            {start.map(|s| kv(tr.sched_start_when, s))}
            {kv(tr.sched_complete_when, cond_label(&j.completion, tr))}
            {tags(constraints)}
            {tags(steps)}
        </div>
    }
}

fn procedure_row(label: &'static str, p: Procedure, tr: &'static Translations) -> impl IntoView {
    let queues: Vec<&str> = [p.pre.as_str(), p.post.as_str()]
        .into_iter()
        .filter(|q| !q.is_empty())
        .map(name_of)
        .collect();
    let value = match (p.enabled, queues.is_empty()) {
        (false, _) => tr.plan_off.to_string(),
        (true, true) => tr.plan_on.to_string(),
        (true, false) => queues.join(" \u{2192} "),
    };
    kv(label, value)
}

fn schedule_view(s: Schedule, tr: &'static Translations) -> impl IntoView {
    let count = s.jobs.len().to_string();
    view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.plan_schedule}</span>
            {(!s.profile.is_empty()).then(|| kv(tr.plan_profile, s.profile.clone()))}
            {kv(tr.sched_jobs_section, count)}
            {s.mosaic.map(|m| kv(tr.tab_mosaic, format!("{} \u{00b7} {}\u{00d7}{}", m.target, m.grid_w, m.grid_h)))}
        </div>
        {s.jobs.into_iter().map(|j| job_card(j, tr)).collect_view()}
        <div class=CARD>
            <span class=CARD_TITLE>{tr.plan_procedures}</span>
            {procedure_row(tr.sched_startup_legend, s.startup, tr)}
            {procedure_row(tr.sched_shutdown_legend, s.shutdown, tr)}
        </div>
    }
}

// ── Sequence ─────────────────────────────────────────────────────────────────

fn frame_row(f: SeqRow, tr: &'static Translations) -> impl IntoView {
    let (icon, color) = frame_type_visual(&f.frame_type);
    let exposure = f.exposure.map_or_else(|| DASH.to_string(), |e| format!("{} s", num(e)));
    let count = f.count.map_or_else(|| DASH.to_string(), |c| c.to_string());
    let mut extras = Vec::new();
    if !f.bin.is_empty() {
        extras.push(format!("{} {}", tr.seq_binning, f.bin));
    }
    if !f.gain.is_empty() {
        extras.push(format!("{} {}", tr.files_gain, f.gain));
    }
    if !f.offset.is_empty() {
        extras.push(format!("{} {}", tr.field_offset, f.offset));
    }
    if !f.iso.is_empty() {
        extras.push(format!("ISO #{}", f.iso));
    }
    view! {
        <div class="flex items-center gap-2 min-h-[44px] md:min-h-9 py-1 border-b border-border-base">
            <span class="inline-flex shrink-0" style=format!("color:{color}") inner_html=icon></span>
            <div class="min-w-0 flex-1 flex flex-col">
                <div class="flex items-center gap-2 min-w-0">
                    <span class="shrink-0 text-sm text-text">{frame_label(&f.frame_type, tr)}</span>
                    {(!f.filter.is_empty()).then(|| view! { <span class=TAG>{f.filter.clone()}</span> })}
                </div>
                {(!extras.is_empty()).then(|| view! {
                    <span class="truncate text-xs text-text-muted">{extras.join(" \u{00b7} ")}</span>
                })}
            </div>
            <span class="shrink-0 font-mono text-sm text-text-blue-bright">{format!("{exposure} \u{00d7} {count}")}</span>
        </div>
    }
}

fn sequence_view(q: Sequence, tr: &'static Translations) -> impl IntoView {
    let total: f64 = q.frames.iter().map(SeqRow::duration_secs).sum();
    let n: u32 = q.frames.iter().filter_map(|f| f.count).sum();
    let unit = if n == 1 { tr.seq_frame_unit } else { tr.seq_frames_unit };
    let target = q.frames.iter().map(|f| f.target.clone()).find(|t| !t.is_empty());
    let dir = q.frames.iter().map(|f| f.dir.clone()).find(|d| !d.is_empty());
    let has_where = target.is_some() || dir.is_some();
    view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.plan_frames}</span>
            {q.frames.into_iter().map(|f| frame_row(f, tr)).collect_view()}
            <div class="flex items-baseline justify-between gap-3 text-sm">
                <span class="text-text-muted">{tr.seq_total}</span>
                <span class="font-mono text-text-blue-bright">{format!("{n} {unit} \u{00b7} {}", fmt_duration(total))}</span>
            </div>
        </div>
        {has_where.then(|| view! {
            <div class=CARD>
                {target.map(|t| kv(tr.files_target, t))}
                {dir.map(|d| kv(tr.seq_dest_folder, d))}
            </div>
        })}
    }
}

// ── Queue ────────────────────────────────────────────────────────────────────

fn task_row(i: usize, t: QueueTask, tr: &'static Translations) -> impl IntoView {
    let script = (t.template_id == "script_execute")
        .then(|| t.params.iter().find(|(k, _)| k == "script_path").map(|(_, v)| v.clone()).unwrap_or_default());
    let title = match &script {
        Some(_) => tr.sched_q_step_script.to_string(),
        None if step_label(tr, &t.template_id) == tr.sched_q_step_unknown => t.template_id.clone(),
        None => step_label(tr, &t.template_id).to_string(),
    };
    let mut details: Vec<String> = Vec::new();
    if let Some(path) = &script {
        details.push(name_of(path).to_string());
    }
    if !t.device.is_empty() {
        details.push(format!("{} {}", tr.plan_device, t.device));
    }
    details.extend(
        t.params
            .iter()
            .filter(|(k, _)| k != "script_path")
            .map(|(k, v)| format!("{} {v}", param_label(tr, k))),
    );
    view! {
        <li class="flex items-baseline gap-3 min-h-[44px] md:min-h-9 py-1 border-b border-border-base last:border-b-0">
            <span class="shrink-0 w-5 text-right font-mono text-xs text-text-faint">{(i + 1).to_string()}</span>
            <div class="min-w-0 flex-1 flex flex-col">
                <span class="text-sm text-text">{title}</span>
                {(!details.is_empty()).then(|| view! {
                    <span class="truncate text-xs text-text-muted">{details.join(" \u{00b7} ")}</span>
                })}
            </div>
        </li>
    }
}

fn queue_view(q: Queue, tr: &'static Translations) -> impl IntoView {
    let empty = q.tasks.is_empty();
    view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.plan_tasks}</span>
            {(!q.title.is_empty()).then(|| kv(tr.sched_q_title, q.title.clone()))}
            {(!q.description.is_empty()).then(|| view! {
                <p class="m-0 text-sm text-text-dim">{q.description.clone()}</p>
            })}
            <ol class="m-0 p-0 list-none flex flex-col">
                {q.tasks.into_iter().enumerate().map(|(i, t)| task_row(i, t, tr)).collect_view()}
            </ol>
            {empty.then(|| view! { <span class="text-sm text-text-faint">{tr.plan_no_tasks}</span> })}
        </div>
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// The formatted file, then its text in a Source disclosure — open for a
/// script (the text *is* the content) or when the file can't be read.
pub(crate) fn content(kind: Kind, text: String, tr: &'static Translations) -> impl IntoView {
    let parsed = parse(kind, &text);
    let open_source = kind == Kind::Scripts || parsed.is_err();
    let body = match parsed {
        Ok(Parsed::Schedule(s)) => schedule_view(s, tr).into_any(),
        Ok(Parsed::Sequence(s)) => sequence_view(s, tr).into_any(),
        Ok(Parsed::Queue(q)) => queue_view(q, tr).into_any(),
        Ok(Parsed::Script) => ().into_any(),
        Err(e) => view! {
            <div class="panel p-3 text-sm text-state-err break-words">{format!("{}: {e}", tr.plan_unreadable)}</div>
        }.into_any(),
    };
    view! {
        {body}
        <details class="panel overflow-hidden" prop:open=open_source>
            <summary class=format!("{CARD_TITLE} cursor-pointer px-3 py-3")>{tr.plan_source}</summary>
            <pre class="m-0 max-h-[50vh] overflow-auto px-3 pb-3 font-mono text-xs text-text-dim whitespace-pre-wrap break-all">{text}</pre>
        </details>
    }
}
