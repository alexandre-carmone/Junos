use crate::i18n::Translations;

pub fn scheduler_status_label(
    tr: &'static Translations,
    status: i64,
) -> (&'static str, &'static str) {
    // Mirrors Ekos::SchedulerState in kstars/ekos/ekos.h:185.
    match status {
        0 => (tr.sched_status_idle, "sched-badge-idle"),
        1 => (tr.sched_status_startup, "sched-badge-running"),
        2 => (tr.sched_status_running, "sched-badge-running"),
        3 => (tr.sched_status_paused, "sched-badge-paused"),
        4 => (tr.sched_status_shutdown, "sched-badge-paused"),
        5 => (tr.sched_status_aborted, "sched-badge-paused"),
        6 => (tr.sched_status_loading, "sched-badge-paused"),
        _ => (tr.sched_status_unknown, "sched-badge-idle"),
    }
}

pub fn job_state_label(tr: &'static Translations, state: i64) -> (&'static str, &'static str) {
    match state {
        0 => (tr.sched_state_idle, "sched-state-queued"),
        1 => (tr.sched_state_evaluating, "sched-state-queued"),
        2 => (tr.sched_state_scheduled, "sched-state-queued"),
        3 => (tr.sched_state_active, "sched-state-active"),
        4 => (tr.sched_state_error, "sched-state-error"),
        5 => (tr.sched_state_aborted, "sched-state-aborted"),
        6 => (tr.sched_state_invalid, "sched-state-error"),
        7 => (tr.sched_state_complete, "sched-state-complete"),
        _ => ("?", "sched-state-queued"),
    }
}

pub fn job_stage_label(tr: &'static Translations, stage: i64) -> &'static str {
    match stage {
        1 => tr.sched_stage_slewing,
        2 => tr.sched_stage_slew_done,
        3 => tr.sched_stage_focusing,
        4 => tr.sched_stage_focus_done,
        5 => tr.sched_stage_aligning,
        6 => tr.sched_stage_align_done,
        7 => tr.sched_stage_reslewing,
        8 => tr.sched_stage_reslew_done,
        9 => tr.sched_stage_post_focus,
        10 => tr.sched_stage_post_focus_done,
        11 => tr.sched_stage_guiding,
        12 => tr.sched_stage_guide_done,
        13 => tr.sched_stage_capturing,
        14 => tr.sched_stage_done,
        _ => "",
    }
}

pub fn ra_to_hms(ra_h: f64) -> String {
    let h = ra_h.floor() as i64;
    let rem = (ra_h - h as f64) * 60.0;
    let m = rem.floor() as i64;
    let s = ((rem - m as f64) * 60.0).round() as i64;
    format!("{:02}h {:02}m {:02}s", h, m, s)
}

pub fn dec_to_dms(dec_d: f64) -> String {
    let sign = if dec_d < 0.0 { "−" } else { "+" };
    let abs = dec_d.abs();
    let d = abs.floor() as i64;
    let rem = (abs - d as f64) * 60.0;
    let m = rem.floor() as i64;
    let s = ((rem - m as f64) * 60.0).round() as i64;
    format!("{}{:02}° {:02}′ {:02}″", sign, d, m, s)
}

pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
