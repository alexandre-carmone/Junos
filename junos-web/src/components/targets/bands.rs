//! What each band shows of an object (`Lines`): chips in the list, rows with
//! the wavelength in the detail sheet, and the filter advice that follows.

use leptos::prelude::*;

use crate::dso_catalog::Lines;
use crate::i18n::Translations;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Ha,
    Oiii,
    Sii,
    Rgb,
}

/// Display order: the narrowband lines by wavelength, then broadband.
pub const BANDS: [Band; 4] = [Band::Oiii, Band::Ha, Band::Sii, Band::Rgb];

impl Band {
    pub fn strength(self, l: Lines) -> u8 {
        match self {
            Band::Ha => l.ha(),
            Band::Oiii => l.oiii(),
            Band::Sii => l.sii(),
            Band::Rgb => l.broad(),
        }
    }

    /// Bit in `TargetPrefs::bands`.
    pub fn bit(self) -> u32 {
        match self {
            Band::Ha => 1,
            Band::Oiii => 2,
            Band::Sii => 4,
            Band::Rgb => 8,
        }
    }

    pub fn label(self, tr: &'static Translations) -> &'static str {
        match self {
            Band::Ha => "H\u{03b1}",
            Band::Oiii => "OIII",
            Band::Sii => "SII",
            Band::Rgb => tr.targets_band_rgb_short,
        }
    }

    /// What the detail sheet says beside the chip.
    fn wavelength(self, tr: &'static Translations) -> String {
        match self {
            Band::Ha => "656 nm".into(),
            Band::Oiii => "501 nm".into(),
            Band::Sii => "672 nm".into(),
            Band::Rgb => format!("{} \u{00b7} 400\u{2013}700 nm", tr.targets_band_rgb),
        }
    }

    /// Chip colours by strength: filled when dominant, outlined when
    /// significant, dashed and dimmed when optional. Literal strings so
    /// Tailwind finds them.
    fn chip(self, strength: u8) -> &'static str {
        match (self, strength) {
            (Band::Ha, 3) => "border-band-ha text-band-ha bg-[color-mix(in_srgb,var(--band-ha)_22%,transparent)]",
            (Band::Ha, 2) => "border-band-ha text-band-ha",
            (Band::Ha, _) => "border-band-ha text-band-ha border-dashed opacity-60",
            (Band::Oiii, 3) => "border-band-oiii text-band-oiii bg-[color-mix(in_srgb,var(--band-oiii)_22%,transparent)]",
            (Band::Oiii, 2) => "border-band-oiii text-band-oiii",
            (Band::Oiii, _) => "border-band-oiii text-band-oiii border-dashed opacity-60",
            (Band::Sii, 3) => "border-band-sii text-band-sii bg-[color-mix(in_srgb,var(--band-sii)_22%,transparent)]",
            (Band::Sii, 2) => "border-band-sii text-band-sii",
            (Band::Sii, _) => "border-band-sii text-band-sii border-dashed opacity-60",
            (Band::Rgb, 3) => "border-band-rgb text-band-rgb bg-[color-mix(in_srgb,var(--band-rgb)_18%,transparent)]",
            (Band::Rgb, 2) => "border-band-rgb text-band-rgb",
            (Band::Rgb, _) => "border-band-rgb text-band-rgb border-dashed opacity-60",
        }
    }

    /// Filled segment of the strength bar.
    fn fill(self) -> &'static str {
        match self {
            Band::Ha => "bg-band-ha",
            Band::Oiii => "bg-band-oiii",
            Band::Sii => "bg-band-sii",
            Band::Rgb => "bg-band-rgb",
        }
    }
}

/// The bands that show at all, strongest first.
fn shown(l: Lines) -> Vec<(Band, u8)> {
    let mut v: Vec<(Band, u8)> = BANDS.iter().map(|&b| (b, b.strength(l))).filter(|(_, s)| *s > 0).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

/// Compact chips for a list row.
pub fn chips(l: Lines, tr: &'static Translations) -> impl IntoView + use<> {
    shown(l)
        .into_iter()
        .map(|(b, s)| view! {
            <span class=format!("inline-flex items-center h-5 px-1.5 rounded-sm border font-mono text-[10px] leading-none {}",
                                b.chip(s))>
                {b.label(tr)}
            </span>
        })
        .collect_view()
}

/// One sentence on which filters suit the object.
pub fn advice(l: Lines, tr: &'static Translations) -> &'static str {
    let (h, o, s, c) = (l.ha(), l.oiii(), l.sii(), l.broad());
    if h == 0 && o == 0 && s == 0 {
        tr.targets_adv_broadband
    } else if c >= 2 && h <= 1 && o <= 1 {
        if h == 1 { tr.targets_adv_broadband_ha } else { tr.targets_adv_broadband }
    } else if (h, o, s, c) == (2, 0, 0, 2) {
        tr.targets_adv_unknown
    } else if c >= 2 {
        tr.targets_adv_mixed
    } else if o >= 3 && o > h {
        tr.targets_adv_oiii
    } else if o >= 2 {
        tr.targets_adv_dual
    } else {
        tr.targets_adv_ha
    }
}

/// Detail sheet: one row per band that shows, with its wavelength and a
/// three-segment strength bar.
pub fn band_rows(l: Lines, tr: &'static Translations) -> impl IntoView + use<> {
    shown(l)
        .into_iter()
        .map(|(b, s)| {
            let word = match s {
                3 => tr.targets_strength_3,
                2 => tr.targets_strength_2,
                _ => tr.targets_strength_1,
            };
            let segments = (1..=3u8)
                .map(|i| {
                    let fill = if i <= s { b.fill() } else { "bg-border-strong" };
                    view! { <span class=format!("w-5 h-1.5 rounded-sm {fill}")></span> }
                })
                .collect_view();
            view! {
                <div class="flex items-center gap-3 min-h-9">
                    <span class=format!("inline-flex items-center justify-center w-12 h-6 shrink-0 rounded-sm border \
                                         font-mono text-xs {}", b.chip(s))>
                        {b.label(tr)}
                    </span>
                    <span class="flex-1 min-w-0 truncate font-mono text-xs text-text-muted">{b.wavelength(tr)}</span>
                    <div class="flex gap-0.5 shrink-0">{segments}</div>
                    <span class="w-24 shrink-0 text-right text-xs text-text-muted">{word}</span>
                </div>
            }
        })
        .collect_view()
}
