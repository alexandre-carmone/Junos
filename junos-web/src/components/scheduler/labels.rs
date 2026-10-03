//! Scheduler tab: turns KStars' numeric state and stage codes, and task-queue
//! template ids, into labels.

use crate::i18n::Translations;

/// Label and badge class for the scheduler state.
pub fn scheduler_status_label(
    tr: &'static Translations,
    status: i64,
) -> (&'static str, &'static str) {
    // Mirrors Ekos::SchedulerState in kstars/ekos/ekos.h:185.
    match status {
        0 => (tr.sched_status_idle, "badge"),
        1 => (tr.sched_status_startup, "badge badge--info"),
        2 => (tr.sched_status_running, "badge badge--ok"),
        3 => (tr.sched_status_paused, "badge badge--warn"),
        4 => (tr.sched_status_shutdown, "badge badge--info"),
        5 => (tr.sched_status_aborted, "badge badge--err"),
        6 => (tr.sched_status_loading, "badge badge--info"),
        _ => (tr.sched_status_unknown, "badge"),
    }
}

/// Label and text color for a job state (`SchedulerJobStatus`, ekos.h).
pub fn job_state_label(tr: &'static Translations, state: i64) -> (&'static str, &'static str) {
    match state {
        0 => (tr.sched_state_idle, "text-text"),
        1 => (tr.sched_state_evaluating, "text-text"),
        2 => (tr.sched_state_scheduled, "text-text"),
        3 => (tr.sched_state_active, "text-state-ok"),
        4 => (tr.sched_state_error, "text-state-err"),
        5 => (tr.sched_state_aborted, "text-state-warn"),
        6 => (tr.sched_state_invalid, "text-state-err"),
        7 => (tr.sched_state_complete, "text-text-blue"),
        _ => ("?", "text-text"),
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

/// A task-queue template id as the user reads it.
pub fn step_label(tr: &'static Translations, id: &str) -> &'static str {
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

/// A task-queue template parameter name as the user reads it.
pub fn param_label(tr: &'static Translations, name: &str) -> &'static str {
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
