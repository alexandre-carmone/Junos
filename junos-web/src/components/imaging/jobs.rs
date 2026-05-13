//! Sequence-job display helpers — status color, detail row extraction, and
//! the chevron-rotation marker class used by collapsible panels.

use crate::i18n::Translations;

use super::fields::value_to_display;

pub(super) fn marker_cls(open: bool) -> &'static str {
    let base = "inline-block w-[10px] text-xs text-[#557] transition-transform duration-[120ms]";
    if open {
        // Trick: prepend the rotation utility; keeping the base unchanged
        // means "▸" rotates 90° to act as the open chevron.
        // (Two leaked &str variants so the closure can return &'static str.)
        "inline-block w-[10px] text-xs text-[#557] transition-transform duration-[120ms] rotate-90"
    } else {
        base
    }
}

pub(super) fn job_status_color(s: &str) -> &'static str {
    let lo = s.to_lowercase();
    if lo == "complete" {
        "var(--state-ok)"
    } else if lo == "capturing" || lo == "in progress" {
        "var(--state-info)"
    } else if lo.contains("abort") || lo.contains("error") {
        "var(--state-err)"
    } else {
        "var(--text-muted)"
    }
}

pub(super) fn job_detail_rows(
    job: &serde_json::Value,
    t: &'static Translations,
) -> Vec<(String, String)> {
    let read = |key: &str| -> String { job_value_display(&job[key]) };
    vec![
        (t.status.to_string(), read("Status")),
        (t.field_frame_type.to_string(), read("Type")),
        (t.field_exposure_s.to_string(), read("Exp")),
        (t.field_count.to_string(), read("Count")),
        (t.field_filter.to_string(), read("Filter")),
        (t.field_bin_x.to_string(), read("Bin")),
        (t.field_gain.to_string(), read("ISO/Gain")),
        (t.field_offset.to_string(), read("Offset")),
        (t.field_encoding.to_string(), read("Encoding")),
        (t.field_format.to_string(), read("Format")),
        (t.field_job_temp_c.to_string(), read("Temperature")),
        (t.field_delay_s.to_string(), read("Delay")),
        (t.field_target_name.to_string(), read("Target")),
        (t.field_directory.to_string(), read("Directory")),
    ]
}

fn job_value_display(v: &serde_json::Value) -> String {
    let s = value_to_display(v);
    if s.is_empty() {
        "—".to_string()
    } else {
        s
    }
}
