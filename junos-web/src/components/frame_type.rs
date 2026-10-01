//! Frame-type (Light / Dark / Bias / Flat) visuals and the color-coded pill
//! picker, shared by the Imaging one-shot panel and the sequence editor.

use std::sync::Arc;

use leptos::prelude::*;

// Light/Dark/Bias/Flat are KStars' canonical frame-type strings (matches
// Scheduler's frame-type select and `SequenceJob` XML). Frame type is a fixed
// KStars enum, not device-reported, so falling back to it is safe until the
// camera publishes `CCD_FRAME_TYPE`.
pub const FRAME_TYPE_FALLBACK: &[&str] = &["Light", "Dark", "Bias", "Flat"];

/// Device-reported frame types, or `FRAME_TYPE_FALLBACK` while the camera
/// hasn't reported any yet.
pub fn frame_type_options(device: Vec<String>) -> Vec<String> {
    if device.is_empty() {
        FRAME_TYPE_FALLBACK.iter().map(|s| s.to_string()).collect()
    } else {
        device
    }
}

// Inline SVGs for the frame-type pills. 16×16 viewBox, stroke uses
// `currentColor` so the pill's color cascade controls the glyph too.
const FRAME_TYPE_ICON_LIGHT: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M8 1.8l1.55 4.05L13.8 6.4l-3.1 2.85.95 4.25L8 11.3 4.35 13.5l.95-4.25L2.2 6.4l4.25-.55z"/></svg>"#;
const FRAME_TYPE_ICON_DARK: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M13.2 9.6A5.4 5.4 0 0 1 6.4 2.8a5.4 5.4 0 1 0 6.8 6.8z"/></svg>"#;
const FRAME_TYPE_ICON_BIAS: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M9 1.5L3.2 9h3.6L7 14.5 12.8 7H9.2z"/></svg>"#;
const FRAME_TYPE_ICON_FLAT: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="2.6"/><path d="M8 1.6v1.8M8 12.6v1.8M1.6 8h1.8M12.6 8h1.8M3.5 3.5l1.3 1.3M11.2 11.2l1.3 1.3M3.5 12.5l1.3-1.3M11.2 4.8l1.3-1.3"/></svg>"#;
const FRAME_TYPE_ICON_OTHER: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor"><circle cx="8" cy="8" r="2.4"/></svg>"#;

/// Maps a frame-type name to (icon SVG, accent color). Unknown names get a
/// neutral dot + muted color so arbitrary device strings still render.
pub fn frame_type_visual(name: &str) -> (&'static str, &'static str) {
    match name {
        "Light" => (FRAME_TYPE_ICON_LIGHT, "var(--accent-cyan)"),
        "Dark"  => (FRAME_TYPE_ICON_DARK,  "#9aa3b2"),
        "Bias"  => (FRAME_TYPE_ICON_BIAS,  "#a285de"),
        "Flat"  => (FRAME_TYPE_ICON_FLAT,  "var(--state-warn)"),
        _       => (FRAME_TYPE_ICON_OTHER, "var(--text-blue)"),
    }
}

/// Row of color-coded icon pills, one per frame type. `current` is the
/// selected name and `on_pick` receives the name of the tapped pill. Four
/// across on wide screens, a 2×2 grid of taller pills on phones.
pub fn frame_type_pills(
    options: impl Fn() -> Vec<String> + Send + Sync + 'static,
    current: impl Fn() -> String + Copy + Send + Sync + 'static,
    on_pick: impl Fn(String) + Send + Sync + 'static,
) -> AnyView {
    let on_pick = Arc::new(on_pick);
    // Memo: the camera snapshot updates often (temperature, state…); only
    // rebuild the pills when the option list itself changes.
    let options = Memo::new(move |_| options());
    view! {
        <div class="grid grid-cols-4 gap-sp-2 max-[479px]:grid-cols-2">
            {move || options.get().into_iter().map(|opt| {
                let (icon, color) = frame_type_visual(&opt);
                let name = opt.clone();
                let active = move || current() == name;
                let active_icon = active.clone();
                // Active pill: filled tint of its own color + ring;
                // inactive: muted border, dim icon, blue label.
                let pill_style = move || if active() {
                    format!(
                        "background:color-mix(in srgb, {color} 22%, transparent);\
                         border-color:{color};color:{color};\
                         box-shadow:inset 0 0 0 1px {color};",
                    )
                } else {
                    "background:transparent;border-color:var(--border-base);color:var(--text-blue);".to_string()
                };
                let icon_style = move || format!(
                    "color:{color};opacity:{};", if active_icon() { "1" } else { "0.6" });
                let pick = on_pick.clone();
                let picked = opt.clone();
                view! {
                    <button
                        type="button"
                        class="flex items-center justify-center gap-[6px] min-w-0 h-[32px] px-sp-2 \
                               rounded-[6px] border text-xs uppercase tracking-[0.06em] \
                               font-medium transition-colors max-[479px]:h-10 \
                               hover:bg-[rgba(255,255,255,0.04)] \
                               focus:outline-none focus:ring-1 focus:ring-offset-0"
                        style=pill_style
                        on:click=move |_| pick(picked.clone())
                    >
                        <span
                            class="inline-flex shrink-0 transition-opacity"
                            style=icon_style
                            inner_html=icon
                        />
                        <span class="truncate">{opt}</span>
                    </button>
                }
            }).collect::<Vec<_>>()}
        </div>
    }.into_any()
}
