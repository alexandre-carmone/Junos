//! Shared Tailwind class fragments and color/icon helpers used across the
//! Imaging tab. Kept here so individual view files don't repeat the same
//! long class strings.

// ── Shared Tailwind class fragments ───────────────────────────────────────────
pub(super) const GHOST_BTN: &str = "btn btn--sm btn-ghost text-text-blue";
pub(super) const ACTION_BTN: &str = "btn btn--sm !border-[color:var(--btn-color,var(--text-blue))] text-[color:var(--btn-color,var(--text-blue))]";
pub(super) const FIELD_INPUT: &str = "input input--sm flex-1 min-w-0 font-mono";
pub(super) const FIELD_LABEL: &str = "basis-[120px] grow-0 shrink-0 text-text-blue overflow-hidden text-ellipsis whitespace-nowrap max-[479px]:basis-auto max-[479px]:text-xs";
pub(super) const PANEL_CLS: &str =
    "border border-border-base bg-[rgba(10,12,20,0.55)] rounded-[3px] overflow-hidden";
pub(super) const SUMMARY_CLS: &str = "list-none cursor-pointer py-sp-2 px-3 text-text-blue text-sm font-bold uppercase tracking-[0.08em] flex items-center gap-sp-2 select-none hover:bg-[rgba(20,24,40,0.7)] [&::-webkit-details-marker]:hidden";
pub(super) const PANEL_BODY: &str = "py-sp-3 px-3 pb-3 border-t border-[#1a1c28]";

pub(super) fn status_color(status: &str) -> &'static str {
    let s = status.to_lowercase();
    if s.contains("error") || s.contains("abort") || s.contains("fail") {
        "var(--state-err)"
    } else if s.contains("complete") {
        "var(--state-ok)"
    } else if s.contains("capturing") || s.contains("progress") {
        "var(--state-info)"
    } else if s.contains("image received") || s.contains("frame") {
        "var(--state-info)"
    } else if s.contains("dither")
        || s.contains("focus")
        || s.contains("filter")
        || s.contains("align")
        || s.contains("temperature")
        || s.contains("rotator")
        || s.contains("meridian")
        || s.contains("calibrat")
    {
        // Transient sub-tasks KStars steps through mid-sequence.
        "var(--accent-cyan)"
    } else if s.contains("waiting") || s.contains("pause") || s.contains("suspend") {
        "var(--state-warn)"
    } else {
        "var(--text-muted)"
    }
}

/// True while the camera/capture pipeline is actively working — used to pulse
/// the status pill and the live exposure bar. Idle/complete/aborted are static.
pub(crate) fn status_is_active(status: &str) -> bool {
    let s = status.to_lowercase();
    s.contains("capturing")
        || s.contains("progress")
        || s.contains("dither")
        || s.contains("focus")
        || s.contains("filter")
        || s.contains("align")
        || s.contains("temperature")
        || s.contains("rotator")
        || s.contains("meridian")
        || s.contains("calibrat")
        || s.contains("waiting")
}
