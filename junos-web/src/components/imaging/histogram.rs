//! Histogram overlay on the Imaging frame.
//!
//! The curve counts the levels of the JPEG on screen, in the browser. KStars
//! auto-stretches every preview (media.cpp:406), so it shows what you see.
//! Two x axes: **Display**, the 256 levels of that image; **ADU**, the raw
//! values within `ADU_SIGMAS` σ of the frame's median (clamped to its min and
//! max — on a min → max axis the sky is a spike a few bins wide, the stars
//! reach 65535), in `ADU_BINS` bins, each display level spread over the raw
//! interval it covers once the stretch is undone; pixels beyond either end
//! pile up in the edge bin, and the tick says so. Up: the share of the pixels
//! (top and dashed middle line, following Lin / Log).
//! Under it sit KStars' statistics of the raw frame (`FrameStats`, in ADU):
//! the median gives the sky background, and Max flags a saturated frame. A tap
//! on them adds Mode, Range, SNR and the clipped shares.

use std::fmt::Write;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, Event, HtmlCanvasElement, HtmlImageElement};

use crate::components::form::CARD_TITLE;
use crate::i18n::{t, Lang};
use crate::ws::FrameStats;

const H: f64 = 100.0;
/// Bins of the ADU axis. More than KStars' 400 — the 256 levels are spread
/// over the raw intervals they cover, so the curve stays smooth.
const ADU_BINS: usize = 512;
/// Half-width of the ADU axis, in σ around the median: wide enough for the
/// sky and the faint stars, narrow enough that the sky keeps a shape.
const ADU_SIGMAS: f64 = 20.0;
/// R, G, B curve colours; a grey frame draws one cyan curve.
const RGB: [&str; 3] = ["var(--state-err)", "var(--state-ok)", "var(--text-blue)"];
/// Scale labels, shadowed so they stay legible over the curve.
const AXIS: &str = "absolute font-mono text-[10px] leading-3 text-text-faint pointer-events-none \
                    [text-shadow:0_0_3px_rgba(0,0,0,0.9)]";
const PILL: &str = "chip h-7 px-2.5 text-xs cursor-pointer disabled:opacity-40 disabled:cursor-default";
const STATS_ROW: &str = "flex flex-wrap items-baseline gap-x-3";

/// 256 bins per channel (R, G, B) across the x axis, counted on every pixel of
/// the preview; `mono` when every pixel is grey.
#[derive(Clone, PartialEq)]
pub struct Histogram {
    bins: [[u32; 256]; 3],
    mono: bool,
    total: u32,
}

impl Histogram {
    /// Count the frame an `<img>` just loaded, from its `load` event.
    pub fn from_load(ev: &Event) -> Option<Self> {
        let img: HtmlImageElement = ev.target()?.dyn_into().ok()?;
        let (w, h) = (img.natural_width(), img.natural_height());
        if w == 0 || h == 0 {
            return None;
        }
        let canvas: HtmlCanvasElement =
            web_sys::window()?.document()?.create_element("canvas").ok()?.dyn_into().ok()?;
        canvas.set_width(w);
        canvas.set_height(h);
        let ctx: CanvasRenderingContext2d = canvas.get_context("2d").ok()??.dyn_into().ok()?;
        ctx.draw_image_with_html_image_element(&img, 0.0, 0.0).ok()?;
        Some(Self::count(&ctx.get_image_data(0.0, 0.0, f64::from(w), f64::from(h)).ok()?.data()))
    }

    fn count(rgba: &[u8]) -> Self {
        let mut bins = [[0u32; 256]; 3];
        let mut mono = true;
        for px in rgba.chunks_exact(4) {
            bins[0][px[0] as usize] += 1;
            bins[1][px[1] as usize] += 1;
            bins[2][px[2] as usize] += 1;
            mono &= px[0] == px[1] && px[1] == px[2];
        }
        Self { bins, mono, total: (rgba.len() / 4) as u32 }
    }

    /// The channels to draw: one grey curve, or R, G, B.
    fn channels(&self) -> &[[u32; 256]] {
        if self.mono { &self.bins[..1] } else { &self.bins }
    }

    /// The display axis: the 256 levels as counted.
    fn display(&self) -> Curves {
        let bins: Vec<Vec<f64>> =
            self.channels().iter().map(|b| b.iter().map(|&c| f64::from(c)).collect()).collect();
        let peak = peak_of(bins.iter().map(|b| &b[1..255]));
        Curves { bins, mono: self.mono, peak, total: self.total, lo: 0.0, hi: 255.0, beyond: (false, false) }
    }

    /// The ADU axis: ±`ADU_SIGMAS` σ around the median, within the frame's
    /// min → max, in `ADU_BINS` bins. Each display level covers a raw
    /// interval (its edges at ±0.5 level, mapped back through the stretch);
    /// its count is spread over the bins that interval overlaps, so a finer
    /// axis never shows a comb. Pixels beyond either end land in the edge bin.
    fn adu(&self, s: &FrameStats) -> Curves {
        let full = full_scale(s);
        let lo = s.min.max(s.median - ADU_SIGMAS * s.stddev);
        let hi = s.max.min(s.median + ADU_SIGMAS * s.stddev).max(lo + 1e-6);
        let scale = ADU_BINS as f64 / (hi - lo);
        let edge = |level: f64| to_adu(level, s, full);
        let channels = self.channels();
        let mut bins = vec![vec![0.0; ADU_BINS]; channels.len()];
        let spread = |bins: &mut [Vec<f64>], level: usize, a: f64, b: f64| {
            // In bin units.
            let (a, b) = ((a.clamp(lo, hi) - lo) * scale, (b.clamp(lo, hi) - lo) * scale);
            let width = b - a;
            for (chan, out) in channels.iter().zip(bins.iter_mut()) {
                let count = f64::from(chan[level]);
                if count == 0.0 {
                    continue;
                }
                if width <= 1e-9 {
                    out[(a as usize).min(ADU_BINS - 1)] += count;
                    continue;
                }
                for i in a.floor() as usize..(b.ceil() as usize).min(ADU_BINS) {
                    let overlap = (b.min(i as f64 + 1.0) - a.max(i as f64)).max(0.0);
                    out[i] += count * overlap / width;
                }
            }
        };
        for level in 1..255 {
            spread(&mut bins, level, edge(level as f64 - 0.5), edge(level as f64 + 0.5));
        }
        // The y scale tops out before the clipping piles go in — and leaves
        // out the edge bins, where the pixels beyond the axis pile up. Level 0
        // holds everything under the stretch's shadows, 255 everything over
        // its highlights. Each gets its neighbour's width, so it stays a spike
        // at that point rather than smearing out to min or max.
        let peak = peak_of(bins.iter().map(|b| &b[1..ADU_BINS - 1]));
        let w0 = edge(1.5) - edge(0.5);
        spread(&mut bins, 0, edge(0.5) - w0, edge(0.5));
        let w255 = edge(254.5) - edge(253.5);
        spread(&mut bins, 255, edge(254.5), edge(254.5) + w255);
        Curves { bins, mono: self.mono, peak, total: self.total, lo, hi, beyond: (lo > s.min, hi < s.max) }
    }

    /// The most populated display level, the clipping piles aside — on the
    /// grey channel, or green for a colour frame.
    fn mode_level(&self) -> usize {
        let chan = if self.mono { &self.bins[0] } else { &self.bins[1] };
        (1..255).max_by_key(|&l| chan[l]).unwrap_or(1)
    }

    /// Share of the pixels clipped at the display bottom (level 0) and top
    /// (255): the highest of the drawn channels.
    fn clipped(&self) -> (f64, f64) {
        let total = f64::from(self.total.max(1));
        let at = |l: usize| self.channels().iter().map(|b| f64::from(b[l])).fold(0.0, f64::max) / total;
        (at(0), at(255))
    }
}

/// What the plot draws: one curve per channel over an x axis `lo` → `hi`.
#[derive(Clone, PartialEq)]
struct Curves {
    bins: Vec<Vec<f64>>,
    mono: bool,
    /// Top of the y scale, in pixels.
    peak: f64,
    total: u32,
    lo: f64,
    hi: f64,
    /// Whether pixels lie beyond the left / right end, piled in the edge bin.
    beyond: (bool, bool),
}

impl Curves {
    /// Width of the SVG view box: the last bin's x.
    fn width(&self) -> f64 {
        (self.bins[0].len() - 1) as f64
    }

    /// The x tick at `at` (0 left, 1 right); an end tick says when the data
    /// goes on beyond it.
    fn x_tick(&self, at: f64) -> String {
        let v = fmt_tick(self.lo + (self.hi - self.lo) * at, self.hi);
        match at {
            a if a <= 0.0 && self.beyond.0 => format!("\u{2039}{v}"),
            a if a >= 1.0 && self.beyond.1 => format!("{v}\u{203a}"),
            _ => v,
        }
    }

    /// Share of the pixels at height `at` (0 bottom, 1 top) of the plot.
    fn share_at(&self, at: f64, log: bool) -> String {
        let count = if log { (self.peak.ln_1p() * at).exp_m1() } else { self.peak * at };
        pct(count / f64::from(self.total.max(1)))
    }
}

/// The highest count of `slices`, at least 1.
fn peak_of<'a>(slices: impl Iterator<Item = &'a [f64]>) -> f64 {
    slices.flatten().copied().fold(0.0, f64::max).max(1.0)
}

/// A share of the pixels as a percentage: `12%`, `1.2%`, `0.12%`.
fn pct(share: f64) -> String {
    match 100.0 * share {
        p if p >= 10.0 => format!("{p:.0}%"),
        p if p >= 1.0 => format!("{p:.1}%"),
        p => format!("{p:.2}%"),
    }
}

/// Top of the frame's sample range, as KStars' stretch takes it (stretch.cpp
/// `getRange`, `recalculateInputRange`): 255 for 8-bit, 1 for a float frame
/// within 0–1, else 65535.
fn full_scale(s: &FrameStats) -> f64 {
    match s.bpp {
        8 => 255.0,
        32 | 64 if s.max <= 1.01 => 1.0,
        _ => 65535.0,
    }
}

/// Raw value behind a display level (0–255). KStars' stretch
/// (stretch.cpp `stretchOneChannel`) is a midtones transfer function
/// MTF(m, x) = (m − 1)x / ((2m − 1)x − m) between shadows and highlights,
/// and the inverse of MTF(m) is MTF(1 − m). KStars only sends the red
/// channel's stretch, so on a colour frame green and blue are approximate.
fn to_adu(level: f64, s: &FrameStats, full: f64) -> f64 {
    let (y, m) = (level / 255.0, 1.0 - s.midtones);
    // NaN (a degenerate midtones of 0 or 1) becomes 0.
    let x = ((m - 1.0) * y / ((2.0 * m - 1.0) * y - m)).max(0.0).min(1.0);
    (s.shadows + x * (s.highlights - s.shadows)) * full
}

/// SVG path of the filled area under `bins`, scaled so `peak` reaches the top.
fn area(bins: &[f64], peak: f64, log: bool) -> String {
    let scale = |c: f64| if log { c.ln_1p() } else { c };
    let top = scale(peak);
    let mut d = format!("M0 {H}");
    for (i, &c) in bins.iter().enumerate() {
        let _ = write!(d, " L{i} {:.1}", H * (1.0 - (scale(c) / top).min(1.0)));
    }
    let _ = write!(d, " L{} {H} Z", bins.len() - 1);
    d
}

/// ADU: whole numbers for integer frames, decimals for float ones (0–1).
fn fmt_adu(v: f64) -> String {
    match v.abs() {
        a if a >= 100.0 || v.fract() == 0.0 => format!("{v:.0}"),
        a if a >= 1.0 => format!("{v:.1}"),
        _ => format!("{v:.3}"),
    }
}

/// An x-axis tick on a scale ending at `hi`: thousands as `16.4k` on a wide
/// scale, whole numbers else, decimals for a float frame.
fn fmt_tick(v: f64, hi: f64) -> String {
    match hi {
        h if h >= 10_000.0 => format!("{:.1}k", v / 1000.0),
        h if h > 1.0 => format!("{v:.0}"),
        _ => format!("{v:.2}"),
    }
}

/// A switch pill; `on` is its pressed state.
fn pill(
    label: impl Fn() -> &'static str + Send + 'static,
    title: impl Fn() -> &'static str + Send + 'static,
    on: Signal<bool>,
    disabled: Signal<bool>,
    click: impl Fn() + 'static,
) -> impl IntoView {
    view! {
        <button type="button"
                class=move || if on.get() { format!("{PILL} btn--active") } else { PILL.to_string() }
                title=title
                aria-pressed=move || on.get().to_string()
                disabled=disabled
                on:click=move |_| click()>
            {label}
        </button>
    }
}

fn stat(label: &'static str, value: String, title: Option<&'static str>) -> impl IntoView {
    view! {
        <span title=title><span class="text-text-faint">{label}</span>" "{value}</span>
    }
}

/// The glass panel along the bottom of the frame: curve and its scale,
/// Display / ADU and Log switches, stats (a tap on them shows them all).
pub fn histogram_panel(
    hist: RwSignal<Option<Histogram>>,
    stats: Signal<Option<FrameStats>>,
    adu: RwSignal<bool>,
    log: RwSignal<bool>,
    expanded: RwSignal<bool>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    // What's drawn, on which axis. ADU needs KStars' stats.
    let shown = Memo::new(move |_| {
        let h = hist.get()?;
        Some(match stats.get().filter(|_| adu.get()) {
            Some(s) => h.adu(&s),
            None => h.display(),
        })
    });

    let curves = move || shown.with(|v| v.as_ref().map(|c| {
        let log = log.get();
        c.bins.iter().zip(RGB).map(|(b, rgb)| {
            let color = if c.mono { "var(--accent-cyan)" } else { rgb };
            view! {
                <path d=area(b, c.peak, log) fill=color fill-opacity="0.35" stroke=color stroke-width="1"
                      stroke-linejoin="round" vector-effect="non-scaling-stroke" class="mix-blend-screen" />
            }
        }).collect::<Vec<_>>()
    }));
    let width = move || shown.with(|v| v.as_ref().map_or(255.0, Curves::width));
    let y_label = move |at: f64| move || shown.with(|v| v.as_ref().map(|c| c.share_at(at, log.get())));
    let x_label = move |at: f64| move || {
        shown.with(|v| v.as_ref().map(|c| c.x_tick(at)).unwrap_or_default())
    };

    // KStars' raw statistics.
    let row1 = move || stats.get().map(|s| {
        let tr = tr();
        // An integer frame at its top value has clipped pixels.
        let saturated = matches!(s.bpp, 8 | 16) && s.max >= f64::from((1u32 << s.bpp) - 1);
        view! {
            {stat(tr.imaging_hist_median, fmt_adu(s.median), None)}
            {stat(tr.imaging_hist_mean, fmt_adu(s.mean), None)}
            {stat("\u{03c3}", fmt_adu(s.stddev), None)}
            {stat("Min", fmt_adu(s.min), None)}
            <span class:text-state-err=saturated title=saturated.then_some(tr.imaging_hist_saturated)>
                {stat("Max", fmt_adu(s.max), None)}
                {saturated.then_some(" \u{26a0}")}
            </span>
        }
    });
    // Derived: the mode (the sky background), range, SNR and the clipped shares.
    let row2 = move || {
        let h = hist.get()?;
        let s = stats.get();
        let tr = tr();
        let level = h.mode_level();
        let mode = match &s {
            Some(s) => fmt_adu(to_adu(level as f64, s, full_scale(s))),
            None => level.to_string(),
        };
        let (low, high) = h.clipped();
        Some(view! {
            {stat(tr.imaging_hist_mode, mode, Some(tr.imaging_hist_mode_title))}
            {s.as_ref().map(|s| stat(tr.imaging_hist_range, fmt_adu(s.max - s.min), None))}
            {s.as_ref().filter(|s| s.stddev > 0.0).map(|s| {
                stat("SNR", format!("{:.1}", (s.mean - s.min) / s.stddev), Some(tr.imaging_hist_snr_title))
            })}
            {stat(tr.imaging_hist_clip_low, pct(low), Some(tr.imaging_hist_clip_low_title))}
            {stat(tr.imaging_hist_clip_high, pct(high), Some(tr.imaging_hist_clip_high_title))}
        })
    };

    let no_stats = Signal::derive(move || stats.with(Option::is_none));
    let is_adu = Signal::derive(move || adu.get() && !no_stats.get());

    view! {
        <div class="panel-glass absolute left-2 right-2 bottom-2 md:left-auto md:w-[360px] px-2.5 py-2 \
                    flex flex-col gap-1.5">
            <div class="flex items-center gap-1">
                <span class=format!("{CARD_TITLE} flex-1 min-w-0 truncate")>{move || tr().imaging_histogram}</span>
                {pill(move || tr().imaging_hist_display, move || tr().imaging_hist_display_title,
                      Signal::derive(move || !is_adu.get()), false.into(), move || adu.set(false))}
                {pill(|| "ADU", move || tr().imaging_hist_adu_title, is_adu, no_stats, move || adu.set(true))}
                <span class="w-1"></span>
                {pill(|| "Log", move || tr().imaging_hist_log_title, log.into(), false.into(),
                      move || log.update(|l| *l = !*l))}
            </div>
            <div class="flex flex-col gap-0.5">
                <div class="relative">
                    <svg viewBox=move || format!("0 0 {} {H}", width()) preserveAspectRatio="none"
                         class="block w-full h-12 md:h-20">
                        {[0.25, 0.5, 0.75].map(|at| view! {
                            <line x1=move || width() * at x2=move || width() * at y1="0" y2=H
                                  stroke="var(--glass-border)" stroke-width="1" vector-effect="non-scaling-stroke" />
                        })}
                        <line x1="0" x2=width y1=H / 2.0 y2=H / 2.0 stroke="var(--glass-border)" stroke-width="1"
                              stroke-dasharray="3 3" vector-effect="non-scaling-stroke" />
                        {curves}
                    </svg>
                    // The y scale on the right: the sky background sits on the left.
                    <span class=format!("{AXIS} right-1 top-0")>{y_label(1.0)}</span>
                    <span class=format!("{AXIS} right-1 top-1/2 -translate-y-full")>{y_label(0.5)}</span>
                </div>
                // The x scale, under the grid lines.
                <div class="relative h-3">
                    <span class=format!("{AXIS} left-0")>{x_label(0.0)}</span>
                    <span class=format!("{AXIS} left-1/4 -translate-x-1/2")>{x_label(0.25)}</span>
                    <span class=format!("{AXIS} left-1/2 -translate-x-1/2")>{x_label(0.5)}</span>
                    <span class=format!("{AXIS} left-3/4 -translate-x-1/2")>{x_label(0.75)}</span>
                    <span class=format!("{AXIS} right-0")>{x_label(1.0)}</span>
                </div>
            </div>
            // The stats: KStars' row always, the derived row on a tap.
            <button type="button"
                    class="w-full text-left flex flex-col gap-0.5 font-mono tabular-nums text-[11px] leading-4 \
                           text-text-muted cursor-pointer"
                    title=move || tr().imaging_hist_more_title
                    aria-expanded=move || expanded.get().to_string()
                    on:click=move |_| expanded.update(|e| *e = !*e)>
                <div class=STATS_ROW>
                    {row1}
                    <span class="ml-auto text-text-faint">
                        {move || if expanded.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </span>
                </div>
                <Show when=move || expanded.get()>
                    <div class=STATS_ROW>{row2}</div>
                </Show>
            </button>
        </div>
    }
}

/// Shows or hides the panel, in the frame's top-right corner.
pub fn toggle_button(show: RwSignal<bool>, lang: RwSignal<Lang>) -> impl IntoView {
    view! {
        <button type="button"
                class=move || format!(
                    "btn-icon absolute top-2 right-2 bg-glass-bg border-glass-border backdrop-blur-glass {}",
                    if show.get() { "text-accent-cyan" } else { "text-text-muted" },
                )
                title=move || t(lang.get()).imaging_histogram
                aria-pressed=move || show.get().to_string()
                on:click=move |_| show.update(|s| *s = !*s)>
            <svg viewBox="0 0 20 20" class="w-5 h-5" fill="currentColor" aria-hidden="true">
                <rect x="3" y="11" width="3" height="6" rx="1" />
                <rect x="8.5" y="4" width="3" height="13" rx="1" />
                <rect x="14" y="8" width="3" height="9" rx="1" />
            </svg>
        </button>
    }
}
