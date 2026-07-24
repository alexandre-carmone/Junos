//! Shared Junos branding: the logo mark and the elevated "section card"
//! primitive used by the redesigned tabs (Mosaic, Polar Align, …).

use leptos::prelude::*;

/// Branded Junos mark: a 3×3 tile grid with the center tile lit as a star,
/// stroked with a cyan→violet gradient. Rendered inline (via `inner_html`) so
/// it carries its own gradient independent of the surrounding text color.
pub const JUNOS_LOGO_SVG: &str = r##"<svg viewBox="0 0 28 28" width="100%" height="100%" fill="none"><defs><linearGradient id="junos-grad" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="var(--accent-cyan)"/><stop offset="1" stop-color="var(--accent-violet)"/></linearGradient></defs><g stroke="url(#junos-grad)" stroke-width="1.4"><rect x="2" y="2" width="6.5" height="6.5" rx="1.5"/><rect x="10.75" y="2" width="6.5" height="6.5" rx="1.5"/><rect x="19.5" y="2" width="6.5" height="6.5" rx="1.5"/><rect x="2" y="10.75" width="6.5" height="6.5" rx="1.5"/><rect x="19.5" y="10.75" width="6.5" height="6.5" rx="1.5"/><rect x="2" y="19.5" width="6.5" height="6.5" rx="1.5"/><rect x="10.75" y="19.5" width="6.5" height="6.5" rx="1.5"/><rect x="19.5" y="19.5" width="6.5" height="6.5" rx="1.5"/></g><path d="M14 9.2 L15.15 12.85 L18.8 14 L15.15 15.15 L14 18.8 L12.85 15.15 L9.2 14 L12.85 12.85 Z" fill="url(#junos-grad)"/></svg>"##;

/// Polar Alignment mark: a polar-scope reticle — concentric rings with N/E/S/W
/// crosshair ticks, a bright center star (the celestial pole) and a small
/// offset dot (Polaris). Cyan→violet gradient, on-brand with the wordmark.
pub const POLAR_LOGO_SVG: &str = r##"<svg viewBox="0 0 28 28" width="100%" height="100%" fill="none"><defs><linearGradient id="polar-grad" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="var(--accent-cyan)"/><stop offset="1" stop-color="var(--accent-violet)"/></linearGradient></defs><g stroke="url(#polar-grad)" stroke-linecap="round"><circle cx="14" cy="14" r="11" stroke-width="1.4"/><circle cx="14" cy="14" r="6" stroke-width="1.2" opacity="0.55"/><path d="M14 1.5 L14 4.5 M14 23.5 L14 26.5 M1.5 14 L4.5 14 M23.5 14 L26.5 14" stroke-width="1.4"/></g><path d="M14 9.5 L15 13 L18.5 14 L15 15 L14 18.5 L13 15 L9.5 14 L13 13 Z" fill="url(#polar-grad)"/><circle cx="20.2" cy="8.4" r="1.15" fill="url(#polar-grad)"/></svg>"##;

/// A gradient "JUNOS" wordmark + subtitle next to a per-tab logo mark.
/// `logo_svg` is the inline SVG mark; `subtitle` is reactive so it can carry a
/// per-tab, translated title.
pub fn junos_header(
    logo_svg: &'static str,
    subtitle: impl Fn() -> String + Send + 'static,
) -> impl IntoView {
    view! {
        <div class="flex items-center gap-3">
            <span class="inline-block w-8 h-8 shrink-0" inner_html=logo_svg></span>
            <div class="flex flex-col leading-tight">
                <span class="text-[16px] font-bold tracking-[0.22em] bg-gradient-to-r from-accent-cyan to-accent-violet bg-clip-text text-transparent">
                    "JUNOS"
                </span>
                <span class="text-[11px] text-text-faint tracking-wide">
                    {move || subtitle()}
                </span>
            </div>
        </div>
    }
}

/// Elevated section card with a colored left accent stripe and an icon+title
/// header. `accent_text`/`accent_bg` are passed as full literal Tailwind class
/// strings (e.g. `"text-accent-cyan"`, `"bg-accent-cyan"`) so the JIT scanner
/// picks them up from source. `icon_svg` inherits the accent via `currentColor`.
pub fn section_card(
    accent_text: &'static str,
    accent_bg: &'static str,
    icon_svg: &'static str,
    title: impl Fn() -> &'static str + Copy + Send + 'static,
    body: impl IntoView + 'static,
) -> impl IntoView {
    view! {
        <div class="shrink-0 relative overflow-hidden bg-bg-elev-1 border border-border-base rounded-lg shadow-2 pl-5 pr-4 py-3 flex flex-col gap-2">
            <div class=format!("absolute left-0 top-0 bottom-0 w-[3px] {accent_bg}")></div>
            <div class="flex items-center gap-2 pb-2 mb-1 border-b border-border-strong">
                <span class=format!("inline-block w-[18px] h-[18px] shrink-0 {accent_text}")
                      inner_html=icon_svg></span>
                <span class=format!("text-sm font-bold tracking-wide {accent_text}")>
                    {move || title()}
                </span>
            </div>
            {body}
        </div>
    }
}
