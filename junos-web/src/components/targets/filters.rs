//! The Targets tab's preferences — sort, filters, Best score weights — each
//! persisted to localStorage (`targets_*`), and the sheet that edits them.

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE, CHIP, FOOTER, LABEL, ROW, SELECT};
use crate::components::sky::persisted;
use crate::dom::event_target_value;
use crate::dso_catalog::{Dso, DsoType};
use crate::i18n::{t, Lang, Translations};

use super::bands::{Band, BANDS};
use super::compute::{Candidate, Weights};

/// Category chips, in this order.
pub const KINDS: [DsoType; 8] = [
    DsoType::Galaxy,
    DsoType::Nebula,
    DsoType::PlanetaryNebula,
    DsoType::SupernovaRemnant,
    DsoType::OpenCluster,
    DsoType::GlobularCluster,
    DsoType::GalaxyCluster,
    DsoType::DarkNebula,
];
const ALL_KINDS: u32 = (1 << KINDS.len()) - 1;

pub fn kind_bit(k: DsoType) -> u32 {
    1 << KINDS.iter().position(|x| *x == k).unwrap_or(0)
}

/// Singular, for a row ("Galaxy").
pub fn kind_label(k: DsoType, tr: &'static Translations) -> &'static str {
    match k {
        DsoType::Galaxy => tr.kind_galaxy,
        DsoType::OpenCluster => tr.kind_open_cluster,
        DsoType::GlobularCluster => tr.kind_globular,
        DsoType::Nebula => tr.kind_nebula,
        DsoType::PlanetaryNebula => tr.kind_planetary,
        DsoType::SupernovaRemnant => tr.kind_snr,
        DsoType::GalaxyCluster => tr.kind_galaxy_cluster,
        DsoType::DarkNebula => tr.kind_dark_nebula,
    }
}

/// Plural, for a category chip ("Galaxies").
fn kind_plural(k: DsoType, tr: &'static Translations) -> &'static str {
    match k {
        DsoType::Galaxy => tr.galaxies,
        DsoType::OpenCluster => tr.open_clusters,
        DsoType::GlobularCluster => tr.globular_clusters,
        DsoType::Nebula => tr.nebulae,
        DsoType::PlanetaryNebula => tr.planetary_nebulae,
        DsoType::SupernovaRemnant => tr.supernova_remnants,
        DsoType::GalaxyCluster => tr.galaxy_clusters,
        DsoType::DarkNebula => tr.dark_nebulae,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Best,
    Alt,
    Moon,
    Size,
    Time,
    Mag,
}

impl Sort {
    pub const ALL: [Sort; 6] = [Sort::Best, Sort::Alt, Sort::Moon, Sort::Size, Sort::Time, Sort::Mag];

    /// Persisted form.
    fn key(self) -> &'static str {
        match self {
            Sort::Best => "best",
            Sort::Alt => "alt",
            Sort::Moon => "moon",
            Sort::Size => "size",
            Sort::Time => "time",
            Sort::Mag => "mag",
        }
    }

    pub fn label(self, tr: &'static Translations) -> &'static str {
        match self {
            Sort::Best => tr.targets_sort_best,
            Sort::Alt => tr.targets_sort_alt,
            Sort::Moon => tr.targets_sort_moon,
            Sort::Size => tr.targets_sort_size,
            Sort::Time => tr.targets_sort_time,
            Sort::Mag => tr.targets_sort_mag,
        }
    }
}

// Defaults: what an imager wants from a first look — high enough for long
// enough, not too faint; everything else open.
const MAX_MAG: f64 = 14.0;
const MIN_ALT: f64 = 30.0;
const MIN_HOURS: f64 = 1.0;
const W_SIZE: f64 = 0.5;

#[derive(Clone, Copy)]
pub struct TargetPrefs {
    sort: RwSignal<String>,
    /// One bit per `KINDS` entry.
    pub kinds: RwSignal<u32>,
    /// `Band::bit`s; 0 = any.
    pub bands: RwSignal<u32>,
    /// IAU abbreviation, "" = all.
    pub con: RwSignal<String>,
    pub max_mag: RwSignal<f64>,
    /// Arcmin.
    pub min_size: RwSignal<f64>,
    /// Degrees; also what "visible" means for the time-up and the curves.
    pub min_alt: RwSignal<f64>,
    /// Degrees, applied when the Moon is up at the object's peak.
    pub min_moon: RwSignal<f64>,
    pub min_hours: RwSignal<f64>,
    w_alt: RwSignal<f64>,
    w_time: RwSignal<f64>,
    w_moon: RwSignal<f64>,
    w_bright: RwSignal<f64>,
    w_size: RwSignal<f64>,
}

/// Plain values of the filters, read once per ranking.
pub struct FilterSet {
    kinds: u32,
    bands: u32,
    max_mag: f64,
    min_size: f64,
    min_moon: f64,
    min_minutes: f64,
}

impl FilterSet {
    /// Every filter but the constellation and the name.
    pub fn passes(&self, c: &Candidate, d: &Dso) -> bool {
        self.kinds & kind_bit(d.kind) != 0
            && (self.bands == 0 || BANDS.iter().any(|b| self.bands & b.bit() != 0 && b.strength(d.lines) >= 2))
            && d.vis_mag as f64 <= self.max_mag
            && d.size_arcmin as f64 >= self.min_size
            && (!c.moon_up || c.moon_sep >= self.min_moon)
            && c.up_minutes >= self.min_minutes
    }
}

impl TargetPrefs {
    pub fn new() -> Self {
        Self {
            sort: persisted("targets_sort", Sort::Best.key().to_string()),
            kinds: persisted("targets_kinds", ALL_KINDS),
            bands: persisted("targets_bands", 0),
            con: persisted("targets_con", String::new()),
            max_mag: persisted("targets_max_mag", MAX_MAG),
            min_size: persisted("targets_min_size", 0.0),
            min_alt: persisted("targets_min_alt", MIN_ALT),
            min_moon: persisted("targets_min_moon", 0.0),
            min_hours: persisted("targets_min_hours", MIN_HOURS),
            w_alt: persisted("targets_w_alt", 1.0),
            w_time: persisted("targets_w_time", 1.0),
            w_moon: persisted("targets_w_moon", 1.0),
            w_bright: persisted("targets_w_bright", 1.0),
            w_size: persisted("targets_w_size", W_SIZE),
        }
    }

    /// Filters and weights back to their defaults; the sort stays.
    fn reset(&self) {
        self.kinds.set(ALL_KINDS);
        self.bands.set(0);
        self.con.set(String::new());
        self.max_mag.set(MAX_MAG);
        self.min_size.set(0.0);
        self.min_alt.set(MIN_ALT);
        self.min_moon.set(0.0);
        self.min_hours.set(MIN_HOURS);
        self.w_alt.set(1.0);
        self.w_time.set(1.0);
        self.w_moon.set(1.0);
        self.w_bright.set(1.0);
        self.w_size.set(W_SIZE);
    }

    pub fn sort(&self) -> Sort {
        let k = self.sort.get();
        Sort::ALL.into_iter().find(|s| s.key() == k).unwrap_or(Sort::Best)
    }

    pub fn set_sort(&self, s: Sort) {
        self.sort.set(s.key().to_string());
    }

    pub fn weights(&self) -> Weights {
        Weights {
            alt: self.w_alt.get(),
            time: self.w_time.get(),
            moon: self.w_moon.get(),
            bright: self.w_bright.get(),
            size: self.w_size.get(),
        }
    }

    pub fn filter_set(&self) -> FilterSet {
        FilterSet {
            kinds: self.kinds.get(),
            bands: self.bands.get(),
            max_mag: self.max_mag.get(),
            min_size: self.min_size.get(),
            min_moon: self.min_moon.get(),
            min_minutes: self.min_hours.get() * 60.0,
        }
    }

    /// How many filters narrow the list, for the Filters button.
    pub fn active_count(&self) -> usize {
        [
            self.kinds.get() != ALL_KINDS,
            self.bands.get() != 0,
            !self.con.get().is_empty(),
            self.max_mag.get() != MAX_MAG,
            self.min_size.get() > 0.0,
            self.min_alt.get() != MIN_ALT,
            self.min_moon.get() > 0.0,
            self.min_hours.get() != MIN_HOURS,
        ]
        .into_iter()
        .filter(|b| *b)
        .count()
    }
}

/// A pill toggling one bit of a mask.
fn bit_chip(mask: RwSignal<u32>, bit: u32, label: impl Fn() -> &'static str + Send + 'static) -> impl IntoView {
    view! {
        <button type="button"
                class=move || if mask.get() & bit != 0 { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                aria-pressed=move || (mask.get() & bit != 0).to_string()
                on:click=move |_| mask.update(|m| *m ^= bit)>
            {move || label()}
        </button>
    }
}

/// Label · slider · value.
fn range_row(
    label: impl Fn() -> &'static str + Send + 'static,
    v: RwSignal<f64>,
    (min, max, step): (f64, f64, f64),
    fmt: fn(f64) -> String,
) -> impl IntoView {
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} w-32 shrink-0")>{move || label()}</span>
            <input type="range" min=min.to_string() max=max.to_string() step=step.to_string()
                   class="flex-1 min-w-0 accent-accent-cyan"
                   prop:value=move || v.get().to_string()
                   on:input=move |ev| {
                       if let Ok(x) = event_target_value(&ev).parse::<f64>() {
                           v.set(x.clamp(min, max));
                       }
                   } />
            <span class="w-14 shrink-0 text-right font-mono text-sm text-text">{move || fmt(v.get())}</span>
        </div>
    }
}

/// The filters sheet's body: categories, wavelengths, constellation, limits,
/// Best score weights, and a footer with Reset. `cons` lists the
/// constellations the other filters leave tonight: (abbreviation, name, count).
pub fn filters_body(prefs: TargetPrefs, cons: Memo<Vec<(String, String, usize)>>, shown: Signal<usize>) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let kind_chips = KINDS
        .into_iter()
        .map(|k| bit_chip(prefs.kinds, kind_bit(k), move || kind_plural(k, tr())))
        .collect_view();
    let band_chips = BANDS
        .into_iter()
        .map(|b: Band| bit_chip(prefs.bands, b.bit(), move || b.label(tr())))
        .collect_view();

    let con = prefs.con;
    let con_options = move || {
        let cur = con.get();
        let mut list = cons.get();
        // Keep the selection listed even when nothing of it is up tonight.
        if !cur.is_empty() && !list.iter().any(|(a, _, _)| *a == cur) {
            list.push((cur.clone(), cur.clone(), 0));
        }
        list.into_iter()
            .map(|(abbr, name, n)| {
                let selected = abbr == cur;
                view! { <option value=abbr prop:selected=selected>{format!("{name} ({n})")}</option> }
            })
            .collect_view()
    };

    let deg = |v: f64| format!("{v:.0}\u{00b0}");
    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().targets_categories}</span>
                <div class="flex flex-wrap gap-1.5">{kind_chips}</div>
            </div>
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().targets_bands}</span>
                <span class="text-xs text-text-muted">{move || tr().targets_bands_hint}</span>
                <div class="flex flex-wrap gap-1.5">{band_chips}</div>
            </div>
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().targets_constellation}</span>
                <select class=format!("{SELECT} w-full")
                        on:change=move |ev| con.set(event_target_value(&ev))>
                    <option value="" prop:selected=move || con.get().is_empty()>
                        {move || tr().targets_all_constellations}
                    </option>
                    {con_options}
                </select>
            </div>
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().targets_limits}</span>
                {range_row(move || tr().targets_max_mag, prefs.max_mag, (4.0, 16.0, 0.5), |v| format!("{v:.1}"))}
                {range_row(move || tr().targets_min_size, prefs.min_size, (0.0, 120.0, 1.0), |v| format!("{v:.0}\u{2032}"))}
                {range_row(move || tr().targets_min_alt, prefs.min_alt, (0.0, 80.0, 5.0), deg)}
                {range_row(move || tr().targets_min_moon, prefs.min_moon, (0.0, 120.0, 5.0), deg)}
                {range_row(move || tr().targets_min_time, prefs.min_hours, (0.0, 8.0, 0.5), |v| format!("{v:.1} h"))}
            </div>
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().targets_weights}</span>
                <span class="text-xs text-text-muted">{move || tr().targets_weights_hint}</span>
                {range_row(move || tr().targets_w_alt, prefs.w_alt, (0.0, 3.0, 0.5), |v| format!("{v:.1}"))}
                {range_row(move || tr().targets_w_time, prefs.w_time, (0.0, 3.0, 0.5), |v| format!("{v:.1}"))}
                {range_row(move || tr().targets_w_moon, prefs.w_moon, (0.0, 3.0, 0.5), |v| format!("{v:.1}"))}
                {range_row(move || tr().targets_w_bright, prefs.w_bright, (0.0, 3.0, 0.5), |v| format!("{v:.1}"))}
                {range_row(move || tr().targets_w_size, prefs.w_size, (0.0, 3.0, 0.5), |v| format!("{v:.1}"))}
            </div>
        </div>
        <div class=FOOTER>
            <span class="flex-1 min-w-0 truncate font-mono text-sm text-text-muted">
                {move || format!("{} {}", shown.get(), tr().targets_count)}
            </span>
            <button class="btn btn-ghost h-11 px-4" on:click=move |_| prefs.reset()>{move || tr().reset}</button>
        </div>
    }
}
