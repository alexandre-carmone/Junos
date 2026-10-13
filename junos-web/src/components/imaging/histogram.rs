//! Histogram overlay on the Imaging frame.
//!
//! The curve counts the levels of the JPEG on screen, in the browser. KStars
//! auto-stretches every preview (media.cpp:406), so it shows what you see.
//! Two x axes: **Display**, the levels 0–255 of that image; **ADU**, a fixed
//! 0 → full-scale axis (65535 for a 16-bit frame), each level mapped back to
//! its raw value by undoing the stretch. Up: the share of the pixels (top and
//! dashed middle line, following Lin / Log).
//! Under it sit KStars' statistics of the raw frame (`FrameStats`, in ADU):
//! the median gives the sky background, and Max flags a saturated frame.

use std::fmt::Write;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, Event, HtmlCanvasElement, HtmlImageElement};

use crate::components::form::CARD_TITLE;
use crate::i18n::{t, Lang};
use crate::ws::FrameStats;

/// Longest side of the copy the levels are counted on — plenty for 256 bins.
const SAMPLE_PX: f64 = 512.0;
const H: f64 = 100.0;
/// R, G, B curve colours; a grey frame draws one cyan curve.
const RGB: [&str; 3] = ["var(--state-err)", "var(--state-ok)", "var(--text-blue)"];
/// Scale labels, shadowed so they stay legible over the curve.
const AXIS: &str = "absolute font-mono text-[10px] leading-3 text-text-faint pointer-events-none \
                    [text-shadow:0_0_3px_rgba(0,0,0,0.9)]";
const PILL: &str = "chip h-7 px-2.5 text-xs cursor-pointer disabled:opacity-40 disabled:cursor-default";

/// 256 bins per channel (R, G, B) across the x axis; `mono` when every pixel
/// is grey.
#[derive(Clone, PartialEq)]
pub struct Histogram {
    bins: [[u32; 256]; 3],
    mono: bool,
}

impl Histogram {
    /// Count the frame an `<img>` just loaded, from its `load` event.
    pub fn from_load(ev: &Event) -> Option<Self> {
        let img: HtmlImageElement = ev.target()?.dyn_into().ok()?;
        let (w, h) = (img.natural_width() as f64, img.natural_height() as f64);
        if w == 0.0 || h == 0.0 {
            return None;
        }
        let k = (SAMPLE_PX / w.max(h)).min(1.0);
        let (cw, ch) = ((w * k).round().max(1.0), (h * k).round().max(1.0));
        let canvas: HtmlCanvasElement =
            web_sys::window()?.document()?.create_element("canvas").ok()?.dyn_into().ok()?;
        canvas.set_width(cw as u32);
        canvas.set_height(ch as u32);
        let ctx: CanvasRenderingContext2d = canvas.get_context("2d").ok()??.dyn_into().ok()?;
        // Nearest neighbour: sample real pixels rather than average them.
        ctx.set_image_smoothing_enabled(false);
        ctx.draw_image_with_html_image_element_and_dw_and_dh(&img, 0.0, 0.0, cw, ch).ok()?;
        Some(Self::count(&ctx.get_image_data(0.0, 0.0, cw, ch).ok()?.data()))
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
        Self { bins, mono }
    }

    /// The same counts on the fixed ADU axis, 0 → `full_scale`: each display
    /// level moves to the bin of its raw value.
    fn on_adu_axis(&self, s: &FrameStats) -> Self {
        let full = full_scale(s);
        let mut bins = [[0u32; 256]; 3];
        for level in 0..256 {
            let bin = ((to_adu(level as f64, s, full) / full * 255.0).round() as usize).min(255);
            for c in 0..3 {
                bins[c][bin] += self.bins[c][level];
            }
        }
        Self { bins, mono: self.mono }
    }

    /// The channels to draw: one grey curve, or R, G, B.
    fn curves(&self) -> &[[u32; 256]] {
        if self.mono { &self.bins[..1] } else { &self.bins }
    }

    /// Top of the y scale, in pixels. Bins 0 and 255 stay out: the stretch
    /// clips the background to 0, saturation piles up in 255, and either spike
    /// would flatten the rest.
    fn peak(&self) -> f64 {
        self.curves().iter().flat_map(|b| b[1..255].iter()).copied().max().unwrap_or(0).max(1) as f64
    }

    /// Share of the pixels at height `at` (0 bottom, 1 top) of the plot.
    fn share_at(&self, at: f64, log: bool) -> String {
        let peak = self.peak();
        let count = if log { (peak.ln_1p() * at).exp_m1() } else { peak * at };
        let total = self.bins[0].iter().sum::<u32>().max(1);
        match 100.0 * count / f64::from(total) {
            p if p >= 10.0 => format!("{p:.0}%"),
            p if p >= 1.0 => format!("{p:.1}%"),
            p => format!("{p:.2}%"),
        }
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
fn area(bins: &[u32; 256], peak: f64, log: bool) -> String {
    let scale = |c: f64| if log { c.ln_1p() } else { c };
    let top = scale(peak);
    let mut d = format!("M0 {H}");
    for (i, &c) in bins.iter().enumerate() {
        let _ = write!(d, " L{i} {:.1}", H * (1.0 - (scale(c as f64) / top).min(1.0)));
    }
    let _ = write!(d, " L255 {H} Z");
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

/// An x-axis tick on a scale ending at `top`: thousands as `16.4k`.
fn fmt_tick(v: f64, top: f64) -> String {
    match top {
        _ if v == 0.0 => "0".into(),
        t if t >= 1000.0 => format!("{:.1}k", v / 1000.0),
        t if t > 1.0 => format!("{v:.0}"),
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

fn stat(label: &'static str, value: f64, class: &'static str) -> impl IntoView {
    view! {
        <span class=class><span class="text-text-faint">{label}</span>" "{fmt_adu(value)}</span>
    }
}

/// The glass panel along the bottom of the frame: curve and its scale,
/// Display / ADU and Log switches, stats.
pub fn histogram_panel(
    hist: RwSignal<Option<Histogram>>,
    stats: Signal<Option<FrameStats>>,
    adu: RwSignal<bool>,
    log: RwSignal<bool>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    // What's drawn, and where its x axis ends. ADU needs KStars' stats.
    let shown = Memo::new(move |_| {
        let h = hist.get()?;
        Some(match stats.get().filter(|_| adu.get()) {
            Some(s) => (h.on_adu_axis(&s), full_scale(&s)),
            None => (h, 255.0),
        })
    });

    let curves = move || shown.with(|v| v.as_ref().map(|(h, _)| {
        let (peak, log) = (h.peak(), log.get());
        h.curves().iter().zip(RGB).map(|(b, rgb)| {
            let color = if h.mono { "var(--accent-cyan)" } else { rgb };
            view! {
                <path d=area(b, peak, log) fill=color fill-opacity="0.35" stroke=color stroke-width="1"
                      stroke-linejoin="round" vector-effect="non-scaling-stroke" class="mix-blend-screen" />
            }
        }).collect::<Vec<_>>()
    }));
    let y_label = move |at: f64| move || shown.with(|v| v.as_ref().map(|(h, _)| h.share_at(at, log.get())));
    let x_label = move |at: f64| move || {
        let top = shown.with(|v| v.as_ref().map_or(255.0, |(_, top)| *top));
        fmt_tick(top * at, top)
    };

    let stats_line = move || stats.get().map(|s| {
        let tr = tr();
        // An integer frame at its top value has clipped pixels.
        let saturated = matches!(s.bpp, 8 | 16) && s.max >= f64::from((1u32 << s.bpp) - 1);
        view! {
            <div class="flex flex-wrap gap-x-3 font-mono tabular-nums text-[11px] leading-4 text-text-muted">
                {stat(tr.imaging_hist_median, s.median, "")}
                {stat(tr.imaging_hist_mean, s.mean, "max-md:hidden")}
                {stat("\u{03c3}", s.stddev, "")}
                {stat("Min", s.min, "max-md:hidden")}
                <span class:text-state-err=saturated title=saturated.then_some(tr.imaging_hist_saturated)>
                    {stat("Max", s.max, "")}
                    {saturated.then_some(" \u{26a0}")}
                </span>
            </div>
        }
    });

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
                    <svg viewBox=format!("0 0 255 {H}") preserveAspectRatio="none" class="block w-full h-12 md:h-20">
                        {[0.25, 0.5, 0.75].map(|at| view! {
                            <line x1=255.0 * at x2=255.0 * at y1="0" y2=H stroke="var(--glass-border)" stroke-width="1"
                                  vector-effect="non-scaling-stroke" />
                        })}
                        <line x1="0" x2="255" y1=H / 2.0 y2=H / 2.0 stroke="var(--glass-border)" stroke-width="1"
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
            {stats_line}
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
