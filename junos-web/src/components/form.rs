//! Form pieces shared by the planning tabs (Mosaic, Scheduler).
//!
//! One touch-sized vocabulary — cards, label-left / control-right rows,
//! toggle pills — plus [`JobOptions`], the per-job scheduler options (startup
//! steps, start / completion, constraints) both tabs edit and send to KStars.

use leptos::prelude::*;

use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang};

pub const CARD: &str = "panel p-3 flex flex-col gap-2";
pub const CARD_TITLE: &str = "text-xs uppercase tracking-[0.06em] font-semibold text-text-muted";
pub const ROW: &str = "flex items-center gap-2 min-h-[44px] md:min-h-9";
pub const LABEL: &str = "min-w-0 truncate text-sm text-text-blue";
pub const NUM: &str = "input input--sm font-mono w-[64px] shrink-0 text-right max-md:h-9";
pub const SELECT: &str = "input input--sm w-[190px] shrink-0 max-md:h-9";
pub const CHIP: &str = "chip h-9 md:h-7 px-3 justify-center cursor-pointer";
pub const CHECK: &str = "w-5 h-5 min-h-0 shrink-0 accent-accent-cyan";
/// Pinned bar under a scrolling form: summary or error left, actions right.
pub const FOOTER: &str = "shrink-0 flex items-center gap-3 px-3 pt-2 \
                          pb-[max(0.5rem,env(safe-area-inset-bottom))] border-t border-border-base bg-bg-elev-1";

/// One row: label left, control right.
pub fn setting_row(
    label: impl Fn() -> &'static str + Send + 'static,
    control: impl IntoView,
) -> impl IntoView {
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label()}</span>
            {control}
        </div>
    }
}

/// A labelled checkbox over the whole row; `on_set` runs after a user toggle
/// (e.g. to push the value to KStars).
pub fn check_row(
    on: RwSignal<bool>,
    label: impl Fn() -> &'static str + Send + 'static,
    on_set: impl Fn(bool) + 'static,
) -> impl IntoView {
    view! {
        <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
            <input type="checkbox" class=CHECK
                   prop:checked=move || on.get()
                   on:change=move |ev| {
                       let v = event_target_checked(&ev);
                       on.set(v);
                       on_set(v);
                   } />
            <span class=LABEL>{move || label()}</span>
        </label>
    }
}

/// A checkbox-gated degree value (min altitude, moon separation / altitude).
pub fn constraint_row(
    on: RwSignal<bool>,
    value: RwSignal<String>,
    label: impl Fn() -> &'static str + Send + 'static,
    max: &'static str,
) -> impl IntoView {
    view! {
        <div class=ROW>
            <label class="flex-1 min-w-0 flex items-center gap-3 cursor-pointer">
                <input type="checkbox" class=CHECK
                       prop:checked=move || on.get()
                       on:change=move |ev| on.set(event_target_checked(&ev)) />
                <span class=LABEL>{move || label()}</span>
            </label>
            <input type="number" min="0" max=max step="1" inputmode="numeric" class=NUM
                   prop:disabled=move || !on.get()
                   prop:value=move || value.get()
                   on:input=move |ev| value.set(event_target_value(&ev)) />
            <span class="w-3 text-sm text-text-muted">"\u{00b0}"</span>
        </div>
    }
}

/// A pill that toggles a bool (startup steps, twilight / horizon).
pub fn toggle_chip(on: RwSignal<bool>, label: impl Fn() -> &'static str + Send + 'static) -> impl IntoView {
    view! {
        <button type="button"
                class=move || if on.get() { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                aria-pressed=move || on.get().to_string()
                on:click=move |_| on.update(|v| *v = !*v)>
            {move || label()}
        </button>
    }
}

/// A sheet over the dimmed tab (Scheduler, Guide): bottom sheet on phones,
/// centered panel on md+. `body` brings its own scroll area and footer. Each
/// sheet is one layer, so a later one (the queue editor) dims and blocks the
/// one under it. `data-sheet` hides the tab wheel meanwhile (`tab_wheel.rs`).
pub fn sheet(
    title: impl Fn() -> &'static str + Send + 'static,
    on_close: impl Fn() + Clone + Send + 'static,
    body: impl IntoView,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let close_backdrop = on_close.clone();
    view! {
        <div class="absolute inset-0 z-[70]" data-sheet="">
            <div class="absolute inset-0 bg-[rgba(2,4,10,0.6)]" on:click=move |_| close_backdrop()></div>
            <div class="panel absolute inset-x-0 bottom-0 max-h-[calc(100%-3.5rem)] rounded-b-none \
                        flex flex-col overflow-hidden \
                        md:inset-x-auto md:left-1/2 md:-translate-x-1/2 md:bottom-auto md:top-6 \
                        md:max-h-[calc(100%-3rem)] md:w-[min(760px,calc(100%-3rem))] md:rounded-lg">
                <div class="shrink-0 flex items-center gap-2 pl-4 pr-2 py-1.5 border-b border-border-base">
                    <span class="flex-1 min-w-0 truncate font-semibold text-text-blue">{move || title()}</span>
                    <button class="btn-icon" title=move || t(lang.get()).info_close on:click=move |_| on_close()>
                        "\u{2716}"
                    </button>
                </div>
                {body}
            </div>
        </div>
    }
}

fn option(value: &'static str, cur: RwSignal<String>, label: impl Fn() -> &'static str + Send + 'static) -> impl IntoView {
    view! { <option value=value prop:selected=move || cur.with(|c| c == value)>{move || label()}</option> }
}

fn datetime_input(v: RwSignal<String>) -> impl IntoView {
    view! {
        <input type="datetime-local" class="input w-full min-w-0 font-mono"
               prop:value=move || v.get()
               on:input=move |ev| v.set(event_target_value(&ev)) />
    }
}

/// Per-job scheduler options, shared by a single job (Scheduler) and every
/// tile of a mosaic. Conditions are strings: start `"asap"` | `"at"`,
/// completion `"sequence"` | `"repeat"` | `"loop"` | `"at"`.
#[derive(Clone, Copy)]
pub struct JobOptions {
    pub track: RwSignal<bool>,
    pub focus: RwSignal<bool>,
    pub align: RwSignal<bool>,
    pub guide: RwSignal<bool>,
    pub start_cond: RwSignal<String>,
    pub start_at: RwSignal<String>,
    pub complete_cond: RwSignal<String>,
    pub complete_count: RwSignal<String>,
    pub complete_at: RwSignal<String>,
    pub use_alt: RwSignal<bool>,
    pub min_alt: RwSignal<String>,
    pub use_moon: RwSignal<bool>,
    pub min_moon: RwSignal<String>,
    pub use_moon_alt: RwSignal<bool>,
    pub moon_max_alt: RwSignal<String>,
    pub twilight: RwSignal<bool>,
    pub horizon: RwSignal<bool>,
}

impl JobOptions {
    pub fn new() -> Self {
        let b = || RwSignal::new(false);
        let s = || RwSignal::new(String::new());
        let o = Self {
            track: b(), focus: b(), align: b(), guide: b(),
            start_cond: s(), start_at: s(),
            complete_cond: s(), complete_count: s(), complete_at: s(),
            use_alt: b(), min_alt: s(), use_moon: b(), min_moon: s(),
            use_moon_alt: b(), moon_max_alt: s(), twilight: b(), horizon: b(),
        };
        o.reset();
        o
    }

    /// The defaults: Track + Guide, start ASAP, run the sequence once,
    /// altitude ≥ 30°, twilight and artificial horizon on.
    pub fn reset(&self) {
        self.track.set(true);
        self.focus.set(false);
        self.align.set(false);
        self.guide.set(true);
        self.start_cond.set("asap".into());
        self.start_at.set(String::new());
        self.complete_cond.set("sequence".into());
        self.complete_count.set("1".into());
        self.complete_at.set(String::new());
        self.use_alt.set(true);
        self.min_alt.set("30".into());
        self.use_moon.set(false);
        self.min_moon.set("0".into());
        self.use_moon_alt.set(false);
        self.moon_max_alt.set("90".into());
        self.twilight.set(true);
        self.horizon.set(true);
    }

    /// `scheduler_set_all_settings` payload for the start condition and the
    /// constraints. They land in KStars' job form, and `scheduler_add_jobs`
    /// snapshots the form into the job it creates.
    pub fn settings_json(&self) -> serde_json::Value {
        let num = |v: RwSignal<String>, default: f64| v.get_untracked().trim().parse::<f64>().unwrap_or(default);
        let at = self.start_cond.get_untracked() == "at";
        serde_json::json!({
            "asapConditionR":        !at,
            "startupTimeConditionR": at,
            "startupTimeEdit":       if at { self.start_at.get_untracked() } else { String::new() },
            "schedulerAltitude":             self.use_alt.get_untracked(),
            "schedulerAltitudeValue":        num(self.min_alt, 30.0),
            "schedulerMoonSeparation":       self.use_moon.get_untracked(),
            "schedulerMoonSeparationValue":  num(self.min_moon, 0.0),
            "schedulerMoonAltitude":         self.use_moon_alt.get_untracked(),
            "schedulerMoonAltitudeMaxValue": num(self.moon_max_alt, 90.0),
            "schedulerTwilight":             self.twilight.get_untracked(),
            "schedulerHorizon":              self.horizon.get_untracked(),
        })
    }

    /// Track / Focus / Align / Guide pills.
    pub fn steps_view(self, lang: RwSignal<Lang>) -> impl IntoView {
        let tr = move || t(lang.get());
        view! {
            <div class="flex flex-wrap gap-1.5">
                {toggle_chip(self.track, move || tr().sched_step_track)}
                {toggle_chip(self.focus, move || tr().sched_step_focus)}
                {toggle_chip(self.align, move || tr().sched_step_align)}
                {toggle_chip(self.guide, move || tr().sched_step_guide)}
            </div>
        }
    }

    /// Start / completion selects. `finish_at` offers KStars' "finish at a
    /// time" — single jobs only: on a mosaic the first tile would run until
    /// then and leave none for the others.
    pub fn conditions_view(self, lang: RwSignal<Lang>, finish_at: bool) -> impl IntoView {
        let tr = move || t(lang.get());
        let o = self;
        view! {
            {setting_row(move || tr().sched_start_when, view! {
                <select class=SELECT on:change=move |ev| o.start_cond.set(event_target_value(&ev))>
                    {option("asap", o.start_cond, move || tr().sched_cond_asap)}
                    {option("at", o.start_cond, move || tr().sched_cond_at_time)}
                </select>
            })}
            <Show when=move || o.start_cond.with(|c| c == "at")>{datetime_input(o.start_at)}</Show>
            {setting_row(move || tr().sched_complete_when, view! {
                <select class=SELECT on:change=move |ev| o.complete_cond.set(event_target_value(&ev))>
                    {option("sequence", o.complete_cond, move || tr().sched_cond_seq)}
                    {option("repeat", o.complete_cond, move || tr().sched_cond_repeat)}
                    {option("loop", o.complete_cond, move || tr().sched_cond_loop)}
                    {finish_at.then(|| option("at", o.complete_cond, move || tr().sched_cond_finish_at))}
                </select>
            })}
            <Show when=move || o.complete_cond.with(|c| c == "repeat")>
                <div class=format!("{ROW} justify-end")>
                    <input type="number" min="1" step="1" inputmode="numeric" class=NUM
                           prop:value=move || o.complete_count.get()
                           on:input=move |ev| o.complete_count.set(event_target_value(&ev)) />
                    <span class="text-sm text-text-muted">{move || tr().sched_times_unit}</span>
                </div>
            </Show>
            <Show when=move || o.complete_cond.with(|c| c == "at")>{datetime_input(o.complete_at)}</Show>
        }
    }

    /// Altitude / moon rows, then the twilight and horizon pills.
    pub fn constraints_view(self, lang: RwSignal<Lang>) -> impl IntoView {
        let tr = move || t(lang.get());
        view! {
            {constraint_row(self.use_alt,      self.min_alt,      move || tr().sched_min_alt,      "90")}
            {constraint_row(self.use_moon,     self.min_moon,     move || tr().sched_moon_sep,     "180")}
            {constraint_row(self.use_moon_alt, self.moon_max_alt, move || tr().sched_moon_max_alt, "90")}
            <div class="flex flex-wrap gap-1.5 pt-1">
                {toggle_chip(self.twilight, move || tr().sched_twilight)}
                {toggle_chip(self.horizon,  move || tr().sched_horizon)}
            </div>
        }
    }
}
