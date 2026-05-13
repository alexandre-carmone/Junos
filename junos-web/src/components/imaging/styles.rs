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
    } else if s.contains("waiting") || s.contains("pause") {
        "var(--state-warn)"
    } else {
        "var(--text-muted)"
    }
}

// Light/Dark/Bias/Flat are KStars' canonical frame-type strings (matches
// Scheduler's frame-type select and `SequenceJob` XML).
pub(super) const FRAME_TYPE_FALLBACK: &[&str] = &["Light", "Dark", "Bias", "Flat"];

// Inline SVGs for the one-shot panel frame-type pills. 16×16 viewBox, stroke
// uses `currentColor` so the pill's color cascade controls the glyph too.
const FRAME_TYPE_ICON_LIGHT: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M8 1.8l1.55 4.05L13.8 6.4l-3.1 2.85.95 4.25L8 11.3 4.35 13.5l.95-4.25L2.2 6.4l4.25-.55z"/></svg>"#;
const FRAME_TYPE_ICON_DARK: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M13.2 9.6A5.4 5.4 0 0 1 6.4 2.8a5.4 5.4 0 1 0 6.8 6.8z"/></svg>"#;
const FRAME_TYPE_ICON_BIAS: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M9 1.5L3.2 9h3.6L7 14.5 12.8 7H9.2z"/></svg>"#;
const FRAME_TYPE_ICON_FLAT: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="2.6"/><path d="M8 1.6v1.8M8 12.6v1.8M1.6 8h1.8M12.6 8h1.8M3.5 3.5l1.3 1.3M11.2 11.2l1.3 1.3M3.5 12.5l1.3-1.3M11.2 4.8l1.3-1.3"/></svg>"#;
const FRAME_TYPE_ICON_OTHER: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor"><circle cx="8" cy="8" r="2.4"/></svg>"#;

/// Maps a frame-type name to (icon SVG, accent color). Unknown names get a
/// neutral dot + muted color so arbitrary device strings still render.
pub(super) fn frame_type_visual(name: &str) -> (&'static str, &'static str) {
    match name {
        "Light" => (FRAME_TYPE_ICON_LIGHT, "var(--accent-cyan)"),
        "Dark"  => (FRAME_TYPE_ICON_DARK,  "#9aa3b2"),
        "Bias"  => (FRAME_TYPE_ICON_BIAS,  "#a285de"),
        "Flat"  => (FRAME_TYPE_ICON_FLAT,  "var(--state-warn)"),
        _       => (FRAME_TYPE_ICON_OTHER, "var(--text-blue)"),
    }
}
