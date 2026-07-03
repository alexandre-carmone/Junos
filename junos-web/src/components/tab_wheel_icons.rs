use crate::Tab;

// Inline SVG icons — `currentColor` so they inherit the button's text color.
// 24x24 viewBox; sized at the call site via the wrapping <span>.
pub fn tab_icon(tab: Tab) -> &'static str {
    match tab {
        // Constellation: connected stars with a bright 4-point focal star
        Tab::Sky => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 6 L10 4 L15 9 L20 5 M15 9 L9 14 L14 18 L20 16" opacity="0.55"/><path d="M15 9 L16.1 11.1 L18.2 9 L16.1 12.1 L15 14 L13.9 12.1 L11.8 9 L13.9 11.1 Z" fill="currentColor" stroke="none"/><circle cx="5" cy="6" r="1" fill="currentColor" stroke="none"/><circle cx="10" cy="4" r="0.9" fill="currentColor" stroke="none"/><circle cx="20" cy="5" r="0.9" fill="currentColor" stroke="none"/><circle cx="9" cy="14" r="1" fill="currentColor" stroke="none"/><circle cx="14" cy="18" r="1" fill="currentColor" stroke="none"/><circle cx="20" cy="16" r="0.9" fill="currentColor" stroke="none"/></svg>"##,
        // German equatorial mount: tripod + pier + tilted RA axis with
        // counterweight (lower-right) and a perpendicular telescope tube
        // (upper-left) pointing skyward — the classic GEM "X" silhouette.
        Tab::Mount => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M7 22 L12 17 L17 22 M12 22 L12 17"/><path d="M12 17 L12 12"/><path d="M18.5 17.5 L7.8 6.8"/><circle cx="18.8" cy="17.8" r="1.8" fill="currentColor" stroke="none"/><path d="M6 9 L11.4 3.6" stroke-width="3.2"/><circle cx="11.7" cy="3.3" r="1" fill="currentColor" stroke="none"/></svg>"##,
        // Concentric focus rings
        Tab::Focus => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="5"/><circle cx="12" cy="12" r="1.5" fill="currentColor"/></svg>"##,
        // Camera body
        Tab::Imaging => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 8 L8 8 L9.5 5.5 L14.5 5.5 L16 8 L20 8 A1 1 0 0 1 21 9 L21 18 A1 1 0 0 1 20 19 L4 19 A1 1 0 0 1 3 18 L3 9 A1 1 0 0 1 4 8 Z"/><circle cx="12" cy="13" r="4"/></svg>"##,
        // Folder with image inside (gallery / file browser)
        Tab::Files => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7 L3 19 A1 1 0 0 0 4 20 L20 20 A1 1 0 0 0 21 19 L21 9 A1 1 0 0 0 20 8 L12 8 L10 5.5 L4 5.5 A1 1 0 0 0 3 6.5 Z"/><circle cx="9" cy="14" r="1.6" fill="currentColor" stroke="none"/><path d="M6 18 L11 13 L14 16 L17 13 L19 15"/></svg>"##,
        // Ursa Minor (Little Dipper): handle curving from Polaris to a 4-star
        // bowl. Polaris (top-left) and Kochab (bowl, lower-right) are drawn
        // larger as the two brightest stars.
        // Stars: Polaris(4,5) - Yildun(7.5,7.5) - eps(10.5,10.5) - zeta(13.5,12.5)
        //        bowl: zeta(13.5,12.5) - eta(18,9.5) - Pherkad(21,15) - Kochab(16,19) - back to zeta
        Tab::PolarAlign => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 5 L7.5 7.5 L10.5 10.5 L13.5 12.5"/><path d="M13.5 12.5 L18 9.5 L21 15 L16 19 Z"/><circle cx="4" cy="5" r="1.6" fill="currentColor" stroke="none"/><circle cx="7.5" cy="7.5" r="0.85" fill="currentColor" stroke="none"/><circle cx="10.5" cy="10.5" r="0.85" fill="currentColor" stroke="none"/><circle cx="13.5" cy="12.5" r="0.95" fill="currentColor" stroke="none"/><circle cx="18" cy="9.5" r="0.95" fill="currentColor" stroke="none"/><circle cx="21" cy="15" r="1.0" fill="currentColor" stroke="none"/><circle cx="16" cy="19" r="1.4" fill="currentColor" stroke="none"/></svg>"##,
        // Guide: locked guide star inside a square tracking reticle (corner brackets)
        Tab::Guide => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7 L3 3 L7 3 M17 3 L21 3 L21 7 M21 17 L21 21 L17 21 M7 21 L3 21 L3 17"/><path d="M12 8.5 L12 15.5 M8.5 12 L15.5 12"/><circle cx="12" cy="12" r="2.2" fill="currentColor" stroke="none"/></svg>"##,
        // Calendar / scheduler
        Tab::Scheduler => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="16" rx="2"/><path d="M3 10 L21 10 M8 3 L8 7 M16 3 L16 7"/><circle cx="9" cy="14" r="0.9" fill="currentColor"/><circle cx="13" cy="14" r="0.9" fill="currentColor"/><circle cx="17" cy="14" r="0.9" fill="currentColor"/></svg>"##,
        // 3x3 mosaic grid
        Tab::Mosaic => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="1"/><path d="M3 9 L21 9 M3 15 L21 15 M9 3 L9 21 M15 3 L15 21"/></svg>"##,
        // Flat panel: an illuminated disc behind a tube — light source + scope
        // aperture suggesting a flat-frame setup.
        Tab::FlatCal => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="6.2"/><path d="M12 3 L12 5 M12 19 L12 21 M3 12 L5 12 M19 12 L21 12 M5.6 5.6 L7 7 M17 17 L18.4 18.4 M5.6 18.4 L7 17 M17 7 L18.4 5.6"/><circle cx="12" cy="12" r="2.4" fill="currentColor" stroke="none" opacity="0.6"/></svg>"##,
        // INDI control panel: three slider tracks with offset thumbs
        Tab::Devices => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M4 7 L20 7 M4 12 L20 12 M4 17 L20 17"/><circle cx="9" cy="7" r="2.2" fill="currentColor" stroke="none"/><circle cx="15.5" cy="12" r="2.2" fill="currentColor" stroke="none"/><circle cx="7" cy="17" r="2.2" fill="currentColor" stroke="none"/></svg>"##,
        // Settings gear
        Tab::Profiles => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33h0a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51h0a1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82v0a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>"##,
    }
}
